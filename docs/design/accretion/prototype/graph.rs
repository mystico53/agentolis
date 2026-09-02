//! Stage 2 and 3 — the road graph and its faces (PRD §7.2 steps 2 and 3).
//!
//! Nodes are welded cell corners; edges are cell boundaries. Two operations
//! turn the raw honeycomb into something grown:
//!
//! * **collapse** — a boundary shorter than `l_min` is contracted to a point,
//!   fusing two three-way junctions into one four-, five- or six-way one. This
//!   is the operation PRD §7.2 calls the organic signature.
//! * **prune** — a deterministic minority of interior boundaries is deleted,
//!   merging the two plots either side into one larger, irregular block and
//!   leaving long through-roads that run several blocks. More pruning on the
//!   planned periphery than in the old town, so the age gradient is structural.
//!
//! Blocks are then recovered honestly, by walking the faces of the planar
//! embedding — not by assuming a face per plot.

use std::collections::BTreeMap;

use polis_layout::determinism::{combine_seeds, fbm2_f64, mix64, SeededRng};

use crate::geom::*;
use crate::voronoi::weld_key;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoadClass {
    Arterial,
    Street,
    Lane,
}

pub struct Graph {
    pub nodes: Vec<P>,
    /// Canonical `(min, max)` endpoint pairs.
    pub edges: Vec<(u32, u32)>,
    /// `adj[n]` = edge ids at `n`, sorted counter-clockwise by outgoing angle.
    pub adj: Vec<Vec<u32>>,
    /// Rendered polyline per edge (a quadratic arc, 5 points).
    pub curve: Vec<Vec<P>>,
    pub class: Vec<RoadClass>,
}

impl Graph {
    pub fn degree(&self, n: usize) -> usize {
        self.adj[n].len()
    }

    pub fn other(&self, e: usize, n: u32) -> u32 {
        let (a, b) = self.edges[e];
        if a == n {
            b
        } else {
            a
        }
    }

    pub fn edge_len(&self, e: usize) -> f64 {
        let (a, b) = self.edges[e];
        dist(self.nodes[a as usize], self.nodes[b as usize])
    }

    /// Build from cell rings by welding shared corners.
    pub fn from_cells(cells: &[Vec<P>], weld_tol: f64) -> Self {
        let mut key_to_node: BTreeMap<(i64, i64), u32> = BTreeMap::new();
        let mut nodes: Vec<P> = Vec::new();
        let mut acc: Vec<(P, u32)> = Vec::new();
        let mut edge_set: BTreeMap<(u32, u32), ()> = BTreeMap::new();

        let mut node_of = |p: P,
                           key_to_node: &mut BTreeMap<(i64, i64), u32>,
                           nodes: &mut Vec<P>,
                           acc: &mut Vec<(P, u32)>| {
            let k = weld_key(p, weld_tol);
            match key_to_node.get(&k) {
                Some(&id) => {
                    let e = &mut acc[id as usize];
                    e.0 = add(e.0, p);
                    e.1 += 1;
                    id
                }
                None => {
                    let id = u32::try_from(nodes.len()).expect("node count fits u32");
                    key_to_node.insert(k, id);
                    nodes.push(p);
                    acc.push((p, 1));
                    id
                }
            }
        };

        for ring in cells {
            let m = ring.len();
            if m < 3 {
                continue;
            }
            let ids: Vec<u32> = ring
                .iter()
                .map(|p| node_of(*p, &mut key_to_node, &mut nodes, &mut acc))
                .collect();
            for i in 0..m {
                let a = ids[i];
                let b = ids[(i + 1) % m];
                if a == b {
                    continue;
                }
                edge_set.insert((a.min(b), a.max(b)), ());
            }
        }
        // Welded node position = mean of the corners that merged into it.
        for (i, n) in nodes.iter_mut().enumerate() {
            let (s, c) = acc[i];
            *n = qp(mul(s, 1.0 / f64::from(c)));
        }
        let edges: Vec<(u32, u32)> = edge_set.into_keys().collect();
        let mut g = Self {
            nodes,
            edges,
            adj: Vec::new(),
            curve: Vec::new(),
            class: Vec::new(),
        };
        g.rebuild_adj();
        g
    }

    fn rebuild_adj(&mut self) {
        let n = self.nodes.len();
        let mut adj: Vec<Vec<u32>> = vec![Vec::new(); n];
        for (i, &(a, b)) in self.edges.iter().enumerate() {
            adj[a as usize].push(i as u32);
            adj[b as usize].push(i as u32);
        }
        for (ni, list) in adj.iter_mut().enumerate() {
            let here = self.nodes[ni];
            // Counter-clockwise by outgoing direction. `atan2` is used only for
            // an ordering, never for a coordinate, so it cannot reach output
            // geometry; ties fall back to edge id for a total order.
            let mut keyed: Vec<(f64, u32)> = list
                .iter()
                .map(|&e| {
                    let o = self.nodes[self.other(e as usize, ni as u32) as usize];
                    let d = sub(o, here);
                    (d[1].atan2(d[0]), e)
                })
                .collect();
            keyed.sort_by(|x, y| {
                x.0.partial_cmp(&y.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(x.1.cmp(&y.1))
            });
            *list = keyed.into_iter().map(|(_, e)| e).collect();
        }
        self.adj = adj;
    }

    /// Contract every edge shorter than `l_min`, shortest first.
    pub fn collapse_short(&mut self, l_min: f64) -> usize {
        let n = self.nodes.len();
        let mut parent: Vec<u32> = (0..n as u32).collect();
        fn find(parent: &mut Vec<u32>, mut x: u32) -> u32 {
            while parent[x as usize] != x {
                parent[x as usize] = parent[parent[x as usize] as usize];
                x = parent[x as usize];
            }
            x
        }
        let mut order: Vec<(f64, usize)> = (0..self.edges.len())
            .map(|e| (self.edge_len(e), e))
            .filter(|(l, _)| *l < l_min)
            .collect();
        order.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });
        // Running merged position, so a chain of short edges collapses to the
        // mean of the whole chain rather than drifting.
        let mut pos: Vec<P> = self.nodes.clone();
        let mut cnt: Vec<u32> = vec![1; n];
        let mut merged = 0usize;
        for (_, e) in order {
            let (a, b) = self.edges[e];
            let ra = find(&mut parent, a);
            let rb = find(&mut parent, b);
            if ra == rb {
                continue;
            }
            if dist(pos[ra as usize], pos[rb as usize]) >= l_min {
                continue;
            }
            let (keep, gone) = if ra < rb { (ra, rb) } else { (rb, ra) };
            let sp = add(
                mul(pos[keep as usize], f64::from(cnt[keep as usize])),
                mul(pos[gone as usize], f64::from(cnt[gone as usize])),
            );
            cnt[keep as usize] += cnt[gone as usize];
            pos[keep as usize] = mul(sp, 1.0 / f64::from(cnt[keep as usize]));
            parent[gone as usize] = keep;
            merged += 1;
        }
        if merged == 0 {
            return 0;
        }
        // Compact.
        let mut remap: Vec<u32> = vec![u32::MAX; n];
        let mut new_nodes: Vec<P> = Vec::new();
        for i in 0..n as u32 {
            let r = find(&mut parent, i);
            if remap[r as usize] == u32::MAX {
                remap[r as usize] = u32::try_from(new_nodes.len()).expect("fits");
                new_nodes.push(qp(pos[r as usize]));
            }
            remap[i as usize] = remap[r as usize];
        }
        let mut set: BTreeMap<(u32, u32), ()> = BTreeMap::new();
        for &(a, b) in &self.edges {
            let na = remap[a as usize];
            let nb = remap[b as usize];
            if na != nb {
                set.insert((na.min(nb), na.max(nb)), ());
            }
        }
        self.nodes = new_nodes;
        self.edges = set.into_keys().collect();
        self.rebuild_adj();
        merged
    }

    /// Delete an edge set (given as edge ids) and compact.
    pub fn delete_edges(&mut self, doomed: &[bool]) -> usize {
        let mut kept: Vec<(u32, u32)> = Vec::with_capacity(self.edges.len());
        let mut removed = 0;
        for (i, e) in self.edges.iter().enumerate() {
            if doomed[i] {
                removed += 1;
            } else {
                kept.push(*e);
            }
        }
        self.edges = kept;
        self.rebuild_adj();
        removed
    }

    /// Drop nodes that no edge touches.
    pub fn compact_nodes(&mut self) {
        let mut remap = vec![u32::MAX; self.nodes.len()];
        let mut new_nodes = Vec::new();
        for i in 0..self.nodes.len() {
            if !self.adj[i].is_empty() {
                remap[i] = u32::try_from(new_nodes.len()).expect("fits");
                new_nodes.push(self.nodes[i]);
            }
        }
        for e in &mut self.edges {
            e.0 = remap[e.0 as usize];
            e.1 = remap[e.1 as usize];
        }
        self.nodes = new_nodes;
        self.rebuild_adj();
    }

    /// Give every road an arc. The bend follows a low-frequency field so
    /// neighbouring roads curve coherently, which is what makes curvature read
    /// as terrain rather than as noise (PRD §7.2 step 1).
    pub fn curve_roads(&mut self, seed: u64, freq: f64, amp: f64) {
        let mut out = Vec::with_capacity(self.edges.len());
        for (i, &(a, b)) in self.edges.iter().enumerate() {
            let pa = self.nodes[a as usize];
            let pb = self.nodes[b as usize];
            let d = sub(pb, pa);
            let l = len(d);
            let m = mul(add(pa, pb), 0.5);
            let nrm = norm(perp(d));
            let field = fbm2_f64(seed, m[0] * freq, m[1] * freq, 2);
            let mut rng = SeededRng::for_seed(
                combine_seeds(mix64(u64::from(a)), mix64(u64::from(b))),
                "road.bend",
            );
            let jitter = rng.next_f64() - 0.5;
            let bend = (field * 1.35 + jitter * 0.30).clamp(-1.0, 1.0) * amp * l;
            let ctl = add(m, mul(nrm, bend * 2.0));
            let mut poly = Vec::with_capacity(5);
            poly.push(qp(pa));
            for k in 1..4 {
                let t = f64::from(k) / 4.0;
                let u = 1.0 - t;
                let q = [
                    u * u * pa[0] + 2.0 * u * t * ctl[0] + t * t * pb[0],
                    u * u * pa[1] + 2.0 * u * t * ctl[1] + t * t * pb[1],
                ];
                poly.push(qp(q));
            }
            poly.push(qp(pb));
            debug_assert_eq!(i, out.len());
            out.push(poly);
        }
        self.curve = out;
        self.class = vec![RoadClass::Lane; self.edges.len()];
    }

    /// Polyline of an edge traversed from `from` to the other end.
    pub fn directed_curve(&self, e: usize, from: u32) -> Vec<P> {
        let c = &self.curve[e];
        if self.edges[e].0 == from {
            c.clone()
        } else {
            let mut v = c.clone();
            v.reverse();
            v
        }
    }

    /// Walk the faces of the planar embedding.
    ///
    /// Returns `(face_halfedges, face_ring)` where a half-edge is encoded as
    /// `2 * edge + (0 if traversed low→high else 1)`. The unbounded face is
    /// dropped.
    pub fn faces(&self) -> Vec<Face> {
        let m = self.edges.len();
        let mut visited = vec![false; 2 * m];
        let mut faces: Vec<Face> = Vec::new();
        // Position of a half-edge in the CCW list of its origin.
        let mut slot: BTreeMap<(u32, u32), usize> = BTreeMap::new();
        for (ni, list) in self.adj.iter().enumerate() {
            for (k, &e) in list.iter().enumerate() {
                slot.insert((ni as u32, e), k);
            }
        }
        for h in 0..2 * m {
            if visited[h] {
                continue;
            }
            let mut ring_h: Vec<usize> = Vec::new();
            let mut cur = h;
            let mut guard = 0usize;
            loop {
                guard += 1;
                if guard > 4 * m + 16 {
                    ring_h.clear();
                    break;
                }
                if visited[cur] {
                    break;
                }
                visited[cur] = true;
                ring_h.push(cur);
                let e = cur / 2;
                let (a, b) = self.edges[e];
                let to = if cur % 2 == 0 { b } else { a };
                // Twin arrives at `to`; take the previous edge in CCW order
                // around `to`, which is the next edge clockwise — the standard
                // face-traversal rule.
                let list = &self.adj[to as usize];
                let k = slot[&(to, e as u32)];
                let kk = (k + list.len() - 1) % list.len();
                let ne = list[kk] as usize;
                let (na, _nb) = self.edges[ne];
                let nh = if na == to { 2 * ne } else { 2 * ne + 1 };
                cur = nh;
                if cur == h {
                    break;
                }
            }
            if ring_h.len() < 3 {
                continue;
            }
            let mut ring: Vec<P> = Vec::new();
            for &hh in &ring_h {
                let e = hh / 2;
                let (a, b) = self.edges[e];
                let from = if hh % 2 == 0 { a } else { b };
                let c = self.directed_curve(e, from);
                for (i, p) in c.iter().enumerate() {
                    if i + 1 < c.len() {
                        ring.push(*p);
                    }
                }
            }
            let ring = dedupe_ring(ring, 1e-7);
            if ring.len() < 3 {
                continue;
            }
            let sa = signed_area2(&ring);
            faces.push(Face {
                half_edges: ring_h,
                ring,
                signed_area2: sa,
            });
        }
        // The unbounded face has the opposite winding to all the bounded ones.
        // Identify it as the single largest-|area| face of the minority sign.
        if faces.is_empty() {
            return faces;
        }
        let pos: f64 = faces.iter().filter(|f| f.signed_area2 > 0.0).count() as f64;
        let neg = faces.len() as f64 - pos;
        let outer_sign = if pos >= neg { -1.0 } else { 1.0 };
        let mut worst: Option<(f64, usize)> = None;
        for (i, f) in faces.iter().enumerate() {
            if f.signed_area2 * outer_sign > 0.0 {
                let a = f.signed_area2.abs();
                if worst.is_none_or(|(w, _)| a > w) {
                    worst = Some((a, i));
                }
            }
        }
        if let Some((_, i)) = worst {
            faces.remove(i);
        }
        for f in &mut faces {
            if f.signed_area2 < 0.0 {
                f.ring.reverse();
                f.signed_area2 = -f.signed_area2;
            }
        }
        faces
    }
}

pub struct Face {
    pub half_edges: Vec<usize>,
    pub ring: Vec<P>,
    pub signed_area2: f64,
}

impl Face {
    pub fn area(&self) -> f64 {
        self.signed_area2.abs() * 0.5
    }
}

/// Choose which interior boundaries to delete so that plots merge into larger,
/// irregular blocks. Never deletes a perimeter edge (that would open the town
/// to the void) and never leaves a node with fewer than two roads.
pub fn choose_prunes(g: &Graph, faces: &[Face], centre: P, radius: f64, seed: u64) -> Vec<bool> {
    let m = g.edges.len();
    let mut face_count = vec![0u8; m];
    for f in faces {
        for &h in &f.half_edges {
            let e = h / 2;
            face_count[e] = face_count[e].saturating_add(1);
        }
    }
    let mut deg: Vec<u32> = (0..g.nodes.len()).map(|i| g.adj[i].len() as u32).collect();
    let mut order: Vec<(u64, usize)> = (0..m)
        .map(|e| {
            let (a, b) = g.edges[e];
            let k = combine_seeds(mix64(u64::from(a)), mix64(u64::from(b)));
            (mix64(k ^ seed), e)
        })
        .collect();
    order.sort_unstable();
    let mut doomed = vec![false; m];
    for (_, e) in order {
        // Both sides must be interior blocks.
        if face_count[e] < 2 {
            continue;
        }
        let (a, b) = g.edges[e];
        if deg[a as usize] < 3 || deg[b as usize] < 3 {
            continue;
        }
        let mid = mul(
            add(g.nodes[a as usize], g.nodes[b as usize]),
            0.5,
        );
        // The old town keeps its fine mesh; the planned periphery merges into
        // bigger blocks. PRD §7.1's age gradient, expressed structurally.
        let t = (dist(mid, centre) / radius.max(1e-6)).clamp(0.0, 1.0);
        let p = (crate::env_f("PRUNE0", 0.05) + crate::env_f("PRUNE1", 0.16) * t * t)
            .clamp(0.0, 1.0);
        let mut rng = SeededRng::for_seed(
            combine_seeds(mix64(u64::from(a)), mix64(u64::from(b))) ^ seed,
            "road.prune",
        );
        if rng.next_f64() < p {
            doomed[e] = true;
            deg[a as usize] -= 1;
            deg[b as usize] -= 1;
        }
    }
    doomed
}

/// Approximate edge betweenness from BFS trees rooted at a deterministic
/// sample of nodes. Drives the road hierarchy (PRD §10) and, with it, the
/// wayfinding skeleton PRD §8 asks to survive decluttering.
pub fn classify_roads(g: &mut Graph, samples: usize) {
    let n = g.nodes.len();
    if n == 0 {
        return;
    }
    let m = g.edges.len();
    let mut load = vec![0u32; m];
    let step = (n / samples.max(1)).max(1);
    let mut roots: Vec<usize> = (0..n).step_by(step).collect();
    roots.truncate(samples.max(1));
    let mut prev_edge = vec![u32::MAX; n];
    let mut seen = vec![u32::MAX; n];
    let mut queue: Vec<u32> = Vec::with_capacity(n);
    for (ri, &r) in roots.iter().enumerate() {
        queue.clear();
        queue.push(r as u32);
        seen[r] = ri as u32;
        prev_edge[r] = u32::MAX;
        let mut head = 0;
        while head < queue.len() {
            let u = queue[head];
            head += 1;
            for &e in &g.adj[u as usize] {
                let v = g.other(e as usize, u);
                if seen[v as usize] != ri as u32 {
                    seen[v as usize] = ri as u32;
                    prev_edge[v as usize] = e;
                    queue.push(v);
                }
            }
        }
        // Every discovered node walks home; edges near the root accumulate.
        for &v in &queue {
            let mut cur = v;
            let mut guard = 0;
            while prev_edge[cur as usize] != u32::MAX && guard < n {
                guard += 1;
                let e = prev_edge[cur as usize];
                load[e as usize] = load[e as usize].saturating_add(1);
                cur = g.other(e as usize, cur);
            }
        }
    }
    let mut sorted: Vec<u32> = load.clone();
    sorted.sort_unstable();
    let a_cut = sorted[(sorted.len() * 88 / 100).min(sorted.len() - 1)];
    let s_cut = sorted[(sorted.len() * 62 / 100).min(sorted.len() - 1)];
    for e in 0..m {
        g.class[e] = if load[e] >= a_cut && a_cut > 0 {
            RoadClass::Arterial
        } else if load[e] >= s_cut && s_cut > 0 {
            RoadClass::Street
        } else {
            RoadClass::Lane
        };
    }
}
