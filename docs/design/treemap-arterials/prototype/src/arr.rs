//! A planar subdivision (arrangement) built by chord-splitting faces.
//!
//! This is the whole trick of the design: the road graph is never "grown" and
//! then hoped to close. It is a subdivision of one polygon, so it is connected,
//! planar and every face is a closed loop, by construction, at every instant.

use std::collections::BTreeMap;

use crate::geom::{self, Pt};

pub type NodeId = u32;
pub type FaceId = u32;

/// Road class, which is also the subdivision level that created the edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// The city rim.
    Rim = 0,
    /// Top-level district border: boulevard.
    Boulevard = 1,
    /// Deeper district border: avenue.
    Avenue = 2,
    /// Inside a district: street.
    Street = 3,
}

#[derive(Debug, Clone)]
pub struct Face {
    pub ring: Vec<NodeId>,
    pub alive: bool,
    /// Index into the district table; `u32::MAX` before districts are assigned.
    pub district: u32,
}

#[derive(Debug, Clone)]
pub struct EdgeInfo {
    pub faces: Vec<FaceId>,
    pub class: Class,
}

#[derive(Debug, Default)]
pub struct Arr {
    pub nodes: Vec<Pt>,
    pub faces: Vec<Face>,
    pub edges: BTreeMap<(NodeId, NodeId), EdgeInfo>,
}

pub fn key(a: NodeId, b: NodeId) -> (NodeId, NodeId) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

impl Arr {
    pub fn from_ring(ring: Vec<Pt>) -> Self {
        let mut arr = Arr::default();
        let ids: Vec<NodeId> = ring
            .into_iter()
            .map(|p| {
                arr.nodes.push(p);
                (arr.nodes.len() - 1) as NodeId
            })
            .collect();
        arr.faces.push(Face {
            ring: ids.clone(),
            alive: true,
            district: u32::MAX,
        });
        for i in 0..ids.len() {
            let a = ids[i];
            let b = ids[(i + 1) % ids.len()];
            arr.edges.insert(
                key(a, b),
                EdgeInfo {
                    faces: vec![0],
                    class: Class::Rim,
                },
            );
        }
        arr
    }

    pub fn poly(&self, f: FaceId) -> Vec<Pt> {
        self.faces[f as usize]
            .ring
            .iter()
            .map(|&n| self.nodes[n as usize])
            .collect()
    }

    fn add_node(&mut self, p: Pt) -> NodeId {
        self.nodes.push(p);
        (self.nodes.len() - 1) as NodeId
    }

    pub fn degree(&self, n: NodeId) -> usize {
        // linear scan is fine at prototype scale; callers cache when hot
        self.edges
            .keys()
            .filter(|&&(a, b)| a == n || b == n)
            .count()
    }

    /// Insert `m` into the middle of edge `(a,b)`, updating every face that uses
    /// it. Both sides of a shared border see the same new vertex, which is why
    /// districts keep exactly-shared boundaries after any amount of splitting.
    fn split_edge(&mut self, a: NodeId, b: NodeId, m: NodeId) {
        let k = key(a, b);
        let info = self
            .edges
            .remove(&k)
            .unwrap_or_else(|| panic!("split_edge on missing edge {k:?}"));
        for &f in &info.faces {
            let ring = &mut self.faces[f as usize].ring;
            let n = ring.len();
            for i in 0..n {
                let u = ring[i];
                let v = ring[(i + 1) % n];
                if (u == a && v == b) || (u == b && v == a) {
                    ring.insert(i + 1, m);
                    break;
                }
            }
        }
        self.edges.insert(
            key(a, m),
            EdgeInfo {
                faces: info.faces.clone(),
                class: info.class,
            },
        );
        self.edges.insert(
            key(m, b),
            EdgeInfo {
                faces: info.faces,
                class: info.class,
            },
        );
    }

    /// Split face `f` with the line `dot(p, n) == s`.
    ///
    /// `snap_r` is PRD §7.2's organic signature applied at construction: when a
    /// new chord endpoint lands within `snap_r` of an existing vertex it reuses
    /// that vertex instead of creating a new one, which is what turns a graph of
    /// pure T-junctions into one with 4- and 5-way junctions.
    pub fn split_face(
        &mut self,
        f: FaceId,
        n: Pt,
        s: f64,
        snap_r: f64,
        class: Class,
    ) -> Option<(FaceId, FaceId)> {
        if !self.faces[f as usize].alive {
            return None;
        }
        let ring = self.faces[f as usize].ring.clone();
        let m = ring.len();
        if m < 3 {
            return None;
        }
        let parent_area = geom::area(&self.poly(f));
        let d: Vec<f64> = ring
            .iter()
            .map(|&v| self.nodes[v as usize].dot(n) - s)
            .collect();
        let eps = 1e-7;
        let sgn: Vec<i32> = d
            .iter()
            .map(|&x| {
                if x > eps {
                    1
                } else if x < -eps {
                    -1
                } else {
                    0
                }
            })
            .collect();

        // Collect crossing sites in ring order: either an existing vertex on the
        // line, or an interior point of an edge.
        enum Hit {
            Vertex(usize),
            Edge(usize, f64),
        }
        let mut hits: Vec<Hit> = Vec::new();
        for i in 0..m {
            if sgn[i] == 0 {
                hits.push(Hit::Vertex(i));
                continue;
            }
            let j = (i + 1) % m;
            if sgn[j] == 0 {
                continue; // will be caught as a vertex hit next iteration
            }
            if sgn[i] * sgn[j] < 0 {
                let t = d[i] / (d[i] - d[j]);
                hits.push(Hit::Edge(i, t));
            }
        }
        if hits.len() != 2 {
            return None;
        }

        // Resolve each hit to a node id.
        let mut resolved: Vec<NodeId> = Vec::with_capacity(2);
        let mut pending: Vec<(NodeId, NodeId, Pt)> = Vec::new();
        for h in &hits {
            match h {
                Hit::Vertex(i) => resolved.push(ring[*i]),
                Hit::Edge(i, t) => {
                    let a = ring[*i];
                    let b = ring[(*i + 1) % m];
                    let pa = self.nodes[a as usize];
                    let pb = self.nodes[b as usize];
                    let p = pa.lerp(pb, *t);
                    if p.dist(pa) <= snap_r {
                        resolved.push(a);
                    } else if p.dist(pb) <= snap_r {
                        resolved.push(b);
                    } else {
                        resolved.push(u32::MAX);
                        pending.push((a, b, p));
                    }
                }
            }
        }
        // Materialise the pending edge splits.
        let mut pi = 0usize;
        for r in resolved.iter_mut() {
            if *r == u32::MAX {
                let (a, b, p) = pending[pi];
                pi += 1;
                let mid = self.add_node(p);
                self.split_edge(a, b, mid);
                *r = mid;
            }
        }
        let (m1, m2) = (resolved[0], resolved[1]);
        if m1 == m2 {
            return None;
        }
        // A chord between two already-adjacent vertices makes a 2-gon.
        if self.edges.contains_key(&key(m1, m2)) {
            return None;
        }

        let ring = self.faces[f as usize].ring.clone();
        let i1 = ring.iter().position(|&v| v == m1)?;
        let i2 = ring.iter().position(|&v| v == m2)?;
        let mut ring_a: Vec<NodeId> = Vec::new();
        let mut i = i1;
        loop {
            ring_a.push(ring[i]);
            if i == i2 {
                break;
            }
            i = (i + 1) % ring.len();
        }
        let mut ring_b: Vec<NodeId> = Vec::new();
        let mut i = i2;
        loop {
            ring_b.push(ring[i]);
            if i == i1 {
                break;
            }
            i = (i + 1) % ring.len();
        }
        if ring_a.len() < 3 || ring_b.len() < 3 {
            return None;
        }
        // Both endpoints can snap onto the *same* straight stretch of boundary,
        // which yields a collinear zero-area face and leaves two coincident
        // paths between the same pair of nodes — the arrangement stops being
        // planar and roads start crossing without a junction. Reject any split
        // that produces a degenerate part and let the caller re-cut at another
        // angle.
        let pa = geom::area(
            &ring_a
                .iter()
                .map(|&n| self.nodes[n as usize])
                .collect::<Vec<_>>(),
        );
        let pb = geom::area(
            &ring_b
                .iter()
                .map(|&n| self.nodes[n as usize])
                .collect::<Vec<_>>(),
        );
        let floor = parent_area * 0.02;
        if pa < floor || pb < floor {
            return None;
        }
        let district = self.faces[f as usize].district;
        self.faces[f as usize].alive = false;
        self.faces.push(Face {
            ring: ring_a.clone(),
            alive: true,
            district,
        });
        let fa = (self.faces.len() - 1) as FaceId;
        self.faces.push(Face {
            ring: ring_b.clone(),
            alive: true,
            district,
        });
        let fb = (self.faces.len() - 1) as FaceId;

        self.edges.insert(
            key(m1, m2),
            EdgeInfo {
                faces: vec![fa, fb],
                class,
            },
        );
        for (ring, nf) in [(&ring_a, fa), (&ring_b, fb)] {
            for i in 0..ring.len() {
                let a = ring[i];
                let b = ring[(i + 1) % ring.len()];
                if let Some(info) = self.edges.get_mut(&key(a, b)) {
                    if !info.faces.contains(&nf) {
                        for slot in info.faces.iter_mut() {
                            if *slot == f {
                                *slot = nf;
                            }
                        }
                    }
                }
            }
        }
        Some((fa, fb))
    }

    /// Delete an interior edge, merging its two faces. Never called on an edge
    /// whose removal would leave a degree-1 node, so the graph keeps minimum
    /// degree 2 and there are no dangling road stubs.
    pub fn remove_edge(&mut self, a: NodeId, b: NodeId) -> Option<FaceId> {
        let k = key(a, b);
        let info = self.edges.get(&k)?.clone();
        if info.faces.len() != 2 {
            return None;
        }
        let (f1, f2) = (info.faces[0], info.faces[1]);
        if f1 == f2 || !self.faces[f1 as usize].alive || !self.faces[f2 as usize].alive {
            return None;
        }
        // orient: find the face whose ring has a -> b consecutive
        let has_dir = |ring: &[NodeId], u: NodeId, v: NodeId| -> Option<usize> {
            let n = ring.len();
            (0..n).find(|&i| ring[i] == u && ring[(i + 1) % n] == v)
        };
        let r1 = self.faces[f1 as usize].ring.clone();
        let r2 = self.faces[f2 as usize].ring.clone();
        let (ra, rb, ua, ub) = if has_dir(&r1, a, b).is_some() && has_dir(&r2, b, a).is_some() {
            (r1, r2, a, b)
        } else if has_dir(&r1, b, a).is_some() && has_dir(&r2, a, b).is_some() {
            (r1, r2, b, a)
        } else {
            return None;
        };
        // ra contains ua->ub, rb contains ub->ua
        let rot = |ring: &[NodeId], start: NodeId| -> Vec<NodeId> {
            let i = ring.iter().position(|&v| v == start).unwrap();
            let mut out = Vec::with_capacity(ring.len());
            for k in 0..ring.len() {
                out.push(ring[(i + k) % ring.len()]);
            }
            out
        };
        let ra_r = rot(&ra, ub); // [ub, ..., ua]
        let rb_r = rot(&rb, ua); // [ua, ..., ub]
        let mut merged = ra_r.clone();
        merged.extend_from_slice(&rb_r[1..rb_r.len() - 1]);
        if merged.len() < 3 {
            return None;
        }
        let district = self.faces[f1 as usize].district;
        self.faces[f1 as usize].alive = false;
        self.faces[f2 as usize].alive = false;
        self.faces.push(Face {
            ring: merged.clone(),
            alive: true,
            district,
        });
        let nf = (self.faces.len() - 1) as FaceId;
        self.edges.remove(&k);
        for i in 0..merged.len() {
            let u = merged[i];
            let v = merged[(i + 1) % merged.len()];
            if let Some(info) = self.edges.get_mut(&key(u, v)) {
                for slot in info.faces.iter_mut() {
                    if *slot == f1 || *slot == f2 {
                        *slot = nf;
                    }
                }
                info.faces.sort_unstable();
                info.faces.dedup();
            }
        }
        Some(nf)
    }

    /// Subdivide every edge longer than `target` into equal parts, so the later
    /// warp bends roads into curves instead of translating straight chords.
    pub fn subdivide_edges(&mut self, target: f64, max_parts: usize) {
        let keys: Vec<(NodeId, NodeId)> = self.edges.keys().copied().collect();
        for (a, b) in keys {
            if !self.edges.contains_key(&(a, b)) {
                continue;
            }
            let pa = self.nodes[a as usize];
            let pb = self.nodes[b as usize];
            let l = pa.dist(pb);
            let parts = ((l / target).round() as i64).clamp(1, max_parts as i64) as usize;
            if parts < 2 {
                continue;
            }
            let mut left = a;
            for i in 1..parts {
                let t = i as f64 / parts as f64;
                let p = pa.lerp(pb, t);
                let m = self.add_node(p);
                self.split_edge(left, b, m);
                left = m;
            }
        }
    }

    pub fn live_faces(&self) -> Vec<FaceId> {
        (0..self.faces.len() as FaceId)
            .filter(|&f| self.faces[f as usize].alive)
            .collect()
    }

    /// Node degrees, computed once.
    pub fn degrees(&self) -> Vec<u32> {
        let mut deg = vec![0u32; self.nodes.len()];
        for &(a, b) in self.edges.keys() {
            deg[a as usize] += 1;
            deg[b as usize] += 1;
        }
        deg
    }

    /// Drop nodes no edge references, renumbering everything.
    pub fn compact(&mut self) {
        let deg = self.degrees();
        let mut remap = vec![u32::MAX; self.nodes.len()];
        let mut new_nodes = Vec::with_capacity(self.nodes.len());
        for i in 0..self.nodes.len() {
            if deg[i] > 0 {
                remap[i] = new_nodes.len() as u32;
                new_nodes.push(self.nodes[i]);
            }
        }
        self.nodes = new_nodes;
        let old = std::mem::take(&mut self.edges);
        for ((a, b), info) in old {
            let (na, nb) = (remap[a as usize], remap[b as usize]);
            if na == u32::MAX || nb == u32::MAX || na == nb {
                continue;
            }
            self.edges.insert(key(na, nb), info);
        }
        for f in self.faces.iter_mut() {
            if !f.alive {
                f.ring.clear();
                continue;
            }
            f.ring = f
                .ring
                .iter()
                .map(|&v| remap[v as usize])
                .filter(|&v| v != u32::MAX)
                .collect();
        }
    }
}

/// The offset `s` along normal `n` that puts `ratio` of the polygon's area on
/// the `dot(p,n) <= s` side. Fixed iteration count keeps it byte-deterministic.
pub fn offset_for_area_ratio(poly: &[Pt], n: Pt, ratio: f64) -> f64 {
    let total = geom::area(poly);
    if total <= 0.0 {
        return 0.0;
    }
    let mut lo = f64::MAX;
    let mut hi = f64::MIN;
    for p in poly {
        let d = p.dot(n);
        lo = lo.min(d);
        hi = hi.max(d);
    }
    for _ in 0..44 {
        let mid = (lo + hi) * 0.5;
        let a = geom::area(&geom::clip_halfplane(poly, n, mid)) / total;
        if a < ratio {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo + hi) * 0.5
}
