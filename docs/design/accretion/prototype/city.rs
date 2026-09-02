//! Stages 3-5 — blocks, lots, buildings, plus districts and streets.

use std::collections::BTreeMap;

use polis_events::LogicalPath;
use polis_layout::determinism::{det_sin_cos, SeededRng};

use crate::accrete::Settlement;
use crate::geom::*;
use crate::graph::{Face, Graph, RoadClass};

#[derive(Debug, Clone)]
pub struct Block {
    pub ring: Vec<P>,
    pub district: Option<u32>,
    pub plots: Vec<u32>,
    /// No plot inside: a square, a green, or a scrap of undeveloped ground.
    pub open: bool,
    /// `node_modules`, `vendor`, `target`: drawn as one dull mass rather than
    /// as individual buildings, because the eye should slide off it (PRD 8).
    pub industrial: bool,
    pub half_edges: Vec<usize>,
}

/// Half-width of the widest drawn road, in units of `sep`. A building must
/// keep at least this far from the block boundary, which *is* the road centre
/// line - that is what makes "no building intersects a road" true by
/// construction rather than by inspection.
pub const ROAD_HALF: f64 = 0.055;

#[derive(Debug, Clone)]
pub struct Lot {
    pub ring: Vec<P>,
    pub block: u32,
    /// Occupants. Usually one; a crowded block puts two houses on a plot rather
    /// than seating a file where a building would stand in the road.
    pub files: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct Building {
    pub ring: Vec<P>,
    pub file: u32,
    pub lot: u32,
    pub monument: bool,
    pub industrial: bool,
    pub height: f64,
    pub fallback: bool,
}

pub struct Street {
    pub polyline: Vec<P>,
    pub weight: u32,
}

pub struct City {
    pub blocks: Vec<Block>,
    pub lots: Vec<Lot>,
    pub buildings: Vec<Building>,
    pub streets: Vec<Street>,
    /// Edge ids that separate two different districts — what "districts share
    /// a border" is measured on.
    pub district_edges: Vec<usize>,
    /// The subset that separates two different *top-level packages*. This is
    /// the wayfinding skeleton PRD 8 asks to survive at every zoom; drawing
    /// every district edge instead turns a repository of many small
    /// directories into a page of outlines.
    pub package_edges: Vec<usize>,
    pub district_of_file: Vec<u32>,
    pub civic_block: Option<u32>,
}

/// A point provably inside a (possibly non-convex) ring, chosen to maximise
/// `min(clearance from the ring, clearance from `avoid` minus `back_off`)`.
///
/// Passing the block ring as `avoid` and the road half-width as `back_off` is
/// what makes "no building touches a road" hold by construction: the building
/// is grown outward from a point that already has the setback.
fn interior_point_avoiding(ring: &[P], avoid: Option<&[P]>, back_off: f64) -> Option<(P, f64)> {
    let score = |t: P| -> f64 {
        let a = dist_to_boundary(ring, t);
        match avoid {
            Some(av) => a.min(dist_to_boundary(av, t) - back_off),
            None => a,
        }
    };
    let c = centroid(ring);
    let mut best: Option<(f64, P)> = if contains(ring, c) {
        Some((score(c), c))
    } else {
        None
    };
    let n = ring.len();
    // Fan-triangle centroids cover any simple ring; for a big ring one probe
    // per edge is plenty and keeps this linear.
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        let jmax = if n > 8 { 1 } else { n };
        for jj in 0..jmax {
            let j = (i + 2 + jj) % n;
            if j == i || j == (i + 1) % n {
                continue;
            }
            let t = mul(add(add(a, b), ring[j]), 1.0 / 3.0);
            if contains(ring, t) {
                let d = score(t);
                if best.is_none_or(|(bd, _)| d > bd) {
                    best = Some((d, t));
                }
            }
        }
        // Also probe halfway from each edge midpoint toward the centroid.
        let t = lerp(mul(add(a, b), 0.5), c, 0.55);
        if contains(ring, t) {
            let d = score(t);
            if best.is_none_or(|(bd, _)| d > bd) {
                best = Some((d, t));
            }
        }
    }
    best.map(|(d, p)| (p, d))
}

fn interior_point(ring: &[P]) -> Option<P> {
    interior_point_avoiding(ring, None, 0.0).map(|(p, _)| p)
}

fn block_index(blocks: &[Block]) -> (BTreeMap<(i32, i32), Vec<u32>>, f64, P) {
    let mut lo = [f64::INFINITY, f64::INFINITY];
    let mut hi = [f64::NEG_INFINITY, f64::NEG_INFINITY];
    let mut per = 0.0;
    for b in blocks {
        let (l, h) = bounds(&b.ring);
        lo[0] = lo[0].min(l[0]);
        lo[1] = lo[1].min(l[1]);
        hi[0] = hi[0].max(h[0]);
        hi[1] = hi[1].max(h[1]);
        per += (h[0] - l[0]).max(h[1] - l[1]);
    }
    let cell = (per / blocks.len().max(1) as f64).max(1e-3);
    let mut idx: BTreeMap<(i32, i32), Vec<u32>> = BTreeMap::new();
    for (i, b) in blocks.iter().enumerate() {
        let (l, h) = bounds(&b.ring);
        let x0 = ((l[0] - lo[0]) / cell).floor() as i32;
        let x1 = ((h[0] - lo[0]) / cell).floor() as i32;
        let y0 = ((l[1] - lo[1]) / cell).floor() as i32;
        let y1 = ((h[1] - lo[1]) / cell).floor() as i32;
        for y in y0..=y1 {
            for x in x0..=x1 {
                idx.entry((x, y)).or_default().push(i as u32);
            }
        }
    }
    (idx, cell, lo)
}

/// Recursive subdivision along the longest axis (PRD §7.2 step 4).
///
/// Once an axis is chosen it is reused for the children until the strip gets
/// too narrow, which is what turns a block into a row of deep, narrow plots
/// facing the street rather than a quad-tree of squares.
#[allow(clippy::too_many_arguments)]
fn subdivide(
    ring: &[P],
    target: f64,
    want: u32,
    axis: Option<P>,
    seed: u64,
    depth: u32,
    min_w: f64,
    out: &mut Vec<Vec<P>>,
) {
    let a = area(ring);
    if depth >= 11 || ring.len() < 3 || (a <= target && want <= 1) || a < target * 0.24 {
        if ring.len() >= 3 {
            out.push(ring.to_vec());
        }
        return;
    }
    let principal = longest_axis(ring);
    let ax = match axis {
        Some(prev) => {
            let (lo, hi) = extent_along(ring, prev);
            let (plo, phi) = extent_along(ring, perp(prev));
            // Keep slicing the same way while the strip is still fat enough.
            if (hi - lo) > (phi - plo) * 0.78 {
                prev
            } else {
                principal
            }
        }
        None => principal,
    };
    let (lo, hi) = extent_along(ring, ax);
    if hi - lo < min_w * 2.0 {
        // Splitting would leave a lot narrower than a building plus its
        // setbacks, which is where slivers and road-crossing buildings come
        // from. Stop instead.
        out.push(ring.to_vec());
        return;
    }
    let mut rng = SeededRng::for_seed(seed, "lot.split");
    // Clamp the jitter so *both* halves stay wider than one buildable strip.
    // Guarding only the parent's extent is what leaves thin end-pieces that no
    // building can stand on without touching the road.
    let span = hi - lo;
    let edge = (min_w / span).clamp(0.0, 0.45);
    let t = (0.5 + (rng.next_f64() - 0.5) * 0.26).clamp(edge, 1.0 - edge);
    let c = lo + span * t;
    let (neg, pos) = split_ring(ring, ax, c);
    let mut pieces: Vec<Vec<P>> = Vec::new();
    pieces.extend(neg);
    pieces.extend(pos);
    pieces.retain(|p| p.len() >= 3 && area(p) > 1e-9);
    if pieces.len() < 2 {
        out.push(ring.to_vec());
        return;
    }
    let total: f64 = pieces.iter().map(|p| area(p)).sum();
    // Deterministic order for the children's seeds.
    let mut keyed: Vec<(i64, i64, usize)> = pieces
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let c = centroid(p);
            ((c[1] * 1e6) as i64, (c[0] * 1e6) as i64, i)
        })
        .collect();
    keyed.sort_unstable();
    for (k, (_, _, i)) in keyed.iter().enumerate() {
        let p = &pieces[*i];
        let frac = if total > 0.0 { area(p) / total } else { 0.5 };
        let w = ((f64::from(want) * frac).round() as u32).max(1);
        subdivide(
            p,
            target,
            w,
            Some(ax),
            seed.wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(k as u64 + 1),
            depth + 1,
            min_w,
            out,
        );
    }
}

pub fn assemble(
    s: &Settlement,
    g: &Graph,
    faces: Vec<Face>,
    imports: &[(u32, u32)],
    sep: f64,
) -> City {
    // --- blocks -----------------------------------------------------------
    let mut blocks: Vec<Block> = faces
        .into_iter()
        .map(|f| Block {
            ring: f.ring,
            district: None,
            plots: Vec::new(),
            open: true,
            industrial: false,
            half_edges: f.half_edges,
        })
        .collect();

    let (idx, cell, lo) = block_index(&blocks);
    let mut plot_block: Vec<Option<u32>> = vec![None; s.plots.len()];
    for (pi, plot) in s.plots.iter().enumerate() {
        let kx = ((plot.pos[0] - lo[0]) / cell).floor() as i32;
        let ky = ((plot.pos[1] - lo[1]) / cell).floor() as i32;
        let mut found = None;
        'outer: for dy in -1..=1 {
            for dx in -1..=1 {
                if let Some(list) = idx.get(&(kx + dx, ky + dy)) {
                    for &bi in list {
                        if contains(&blocks[bi as usize].ring, plot.pos) {
                            found = Some(bi);
                            break 'outer;
                        }
                    }
                }
            }
        }
        if let Some(bi) = found {
            plot_block[pi] = Some(bi);
            blocks[bi as usize].plots.push(pi as u32);
        }
    }

    for b in &mut blocks {
        if b.plots.is_empty() {
            continue;
        }
        b.open = false;
        let mut tally: BTreeMap<u32, u32> = BTreeMap::new();
        for &pi in &b.plots {
            *tally.entry(s.plots[pi as usize].district).or_insert(0) += 1;
        }
        // Majority district; ties by district index, which is creation order.
        let best = tally
            .iter()
            .max_by_key(|(d, c)| (**c, std::cmp::Reverse(**d)))
            .map(|(d, _)| *d);
        b.district = best;
        b.industrial = b
            .plots
            .iter()
            .filter(|&&pi| {
                s.plots[pi as usize].files.iter().any(|&fi| s.files[fi as usize].industrial)
            })
            .count()
            * 2
            > b.plots.len();
    }

    // The civic square: the open block nearest the historic centre, or the
    // block holding the root district's oldest ground (PRD §8).
    let civic = {
        let mut best: Option<(f64, u32)> = None;
        for (i, b) in blocks.iter().enumerate() {
            if !b.open {
                continue;
            }
            let c = centroid(&b.ring);
            let d = len(c);
            let a = area(&b.ring);
            if a < sep * sep * 0.35 {
                continue;
            }
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, i as u32));
            }
        }
        best.filter(|(d, _)| *d < sep * 4.0).map(|(_, i)| i)
    };

    // --- lots -------------------------------------------------------------
    let road_half = sep * ROAD_HALF;
    // The parcel grain is a fixed fraction of the median block, so a 90-file
    // village and a 5000-file city have the same lot texture: a block is always
    // divided into a handful of plots, and the ones nobody occupies read as
    // yards and gardens rather than as one enormous empty lot.
    let grain = {
        let mut a: Vec<f64> = blocks.iter().map(|b| area(&b.ring)).collect();
        a.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
        let med = if a.is_empty() { 1.0 } else { a[a.len() / 2] };
        let total: f64 = a.iter().sum();
        // Aim for roughly `LOT_SLACK` plots per file, so occupancy is the same
        // at 90 files and at 5000: some plots are built on, the rest are yards.
        let want = total / (crate::env_f("LOT_SLACK", 1.8) * s.files.len().max(1) as f64);
        want.clamp(med * 0.07, med * 0.50)
    };
    let mut lots: Vec<Lot> = Vec::new();
    for (bi, b) in blocks.iter().enumerate() {
        if Some(bi as u32) == civic {
            continue;
        }
        let mut files: Vec<u32> = Vec::new();
        for &pi in &b.plots {
            files.extend_from_slice(&s.plots[pi as usize].files);
        }
        if files.is_empty() {
            continue;
        }
        // Growth order within the block, so the oldest file takes the first lot.
        files.sort_by_key(|&f| {
            (
                s.files[f as usize].growth_index,
                s.files[f as usize].path.as_str().to_owned(),
            )
        });
        let a = area(&b.ring);
        let want = u32::try_from(files.len().max((a / grain).round() as usize))
            .unwrap_or(u32::MAX);
        let target = grain.min((a / f64::from(want.max(1))) * 1.05);
        let mut rings: Vec<Vec<P>> = Vec::new();
        let seed = s.files[files[0] as usize].path.layout_seed() ^ (bi as u64).wrapping_mul(31);
        subdivide(
            &b.ring,
            target,
            want,
            None,
            seed,
            0,
            sep * ROAD_HALF * 3.6,
            &mut rings,
        );
        rings.retain(|r| area(r) > 1e-6);

        // A lot is *viable* when a building can stand on it with the full road
        // setback. Files are seated only on viable lots; the rest stay vacant
        // and read as yards and gardens (PRD 7.5's vacant lots, for free).
        let min_r = road_half * 1.25;
        let mut viable: Vec<((i64, i64, i64), usize, f64)> = Vec::new();
        let mut spare: Vec<((i64, i64), usize)> = Vec::new();
        for (i, r) in rings.iter().enumerate() {
            let c = centroid(r);
            let key = ((c[1] * 1e5) as i64, (c[0] * 1e5) as i64);
            match interior_point_avoiding(r, Some(&b.ring), road_half) {
                Some((q, clear)) if clear >= min_r => {
                    // Street frontage first: a plot on the block edge is built
                    // on before one buried in the middle, so houses line the
                    // roads and the interior of a block stays open. That is
                    // what a town actually does, and it is what makes a road
                    // look like a street rather than a boundary.
                    let front = (dist_to_boundary(&b.ring, q) / grain.sqrt()).round() as i64;
                    viable.push(((front, key.0, key.1), i, area(r)));
                }
                _ => spare.push((key, i)),
            }
        }
        viable.sort_by(|x, y| x.0.cmp(&y.0));
        spare.sort_unstable();

        let base_lot = lots.len();
        let mut lot_of_ring: Vec<u32> = Vec::new();
        for (_, ri, _) in &viable {
            lot_of_ring.push(u32::try_from(lots.len()).expect("fits"));
            lots.push(Lot {
                ring: rings[*ri].clone(),
                block: bi as u32,
                files: Vec::new(),
            });
        }
        let n_viable = viable.len();
        for (_, ri) in &spare {
            lots.push(Lot {
                ring: rings[*ri].clone(),
                block: bi as u32,
                files: Vec::new(),
            });
        }

        if n_viable == 0 {
            // Nothing here can hold a building at all: seat the files on the
            // widest non-viable lot so no file is lost, and let the metric
            // count them.
            if !lots[base_lot..].is_empty() {
                let mut best = base_lot;
                for i in base_lot..lots.len() {
                    if area(&lots[i].ring) > area(&lots[best].ring) {
                        best = i;
                    }
                }
                lots[best].files = files;
            }
            continue;
        }
        // Seat files on viable lots in spatial order, oldest file first. When
        // there are more files than viable lots the surplus doubles up on the
        // roomiest ones - two houses on one plot, which is what a crowded
        // quarter actually does, and keeps every building off the road.
        let mut by_room: Vec<usize> = (0..n_viable).collect();
        by_room.sort_by(|&x, &y| {
            viable[y]
                .2
                .partial_cmp(&viable[x].2)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(x.cmp(&y))
        });
        for (k, fi) in files.iter().enumerate() {
            let slot = if k < n_viable {
                k
            } else {
                by_room[(k - n_viable) % n_viable]
            };
            lots[lot_of_ring[slot] as usize].files.push(*fi);
        }
    }

    // --- buildings --------------------------------------------------------
    let c_blocks = &blocks;
    let mut buildings: Vec<Building> = Vec::new();
    for li in 0..lots.len() {
        let n_here = lots[li].files.len();
        if n_here == 0 {
            continue;
        }
        let lot_ring = lots[li].ring.clone();
        let blk_ring = c_blocks[lots[li].block as usize].ring.clone();
        let la = area(&lot_ring);
        if la < 1e-9 {
            continue;
        }
        // Two or more occupants share the plot: divide the buildable interior
        // between them before any footprint is drawn.
        let parcels: Vec<Vec<P>> = if n_here == 1 {
            vec![lot_ring.clone()]
        } else {
            let mut out = Vec::new();
            let seed = s.files[lots[li].files[0] as usize].path.layout_seed() ^ 0x5AFE;
            subdivide(
                &lot_ring,
                la / n_here as f64 * 0.92,
                u32::try_from(n_here).unwrap_or(u32::MAX),
                None,
                seed,
                0,
                road_half * 1.2,
                &mut out,
            );
            out.retain(|r| area(r) > 1e-7);
            let mut keyed: Vec<((i64, i64), usize)> = out
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let c = centroid(r);
                    (((c[1] * 1e5) as i64, (c[0] * 1e5) as i64), i)
                })
                .collect();
            keyed.sort_unstable();
            let ordered: Vec<Vec<P>> = keyed.into_iter().map(|(_, i)| out[i].clone()).collect();
            if ordered.is_empty() {
                vec![lot_ring.clone()]
            } else {
                ordered
            }
        };
        let files_here = lots[li].files.clone();
        for (k, fi) in files_here.iter().enumerate() {
            let parcel = &parcels[k.min(parcels.len() - 1)];
            let f = &s.files[*fi as usize];
            let pa = area(parcel);
            if pa < 1e-9 {
                continue;
            }
            let (anchor, clearance) =
                interior_point_avoiding(parcel, Some(&blk_ring), road_half)
                    .unwrap_or_else(|| (centroid(parcel), 0.0));
            // Footprint proportional to sqrt(size_bytes), clamped into the lot
            // (PRD 7.3).
            let want = ((f.size_bytes.max(1) as f64).sqrt() * 0.0125).clamp(0.02, 1e9);
            let want = want
                .min(pa * crate::env_f("BLDG_MAX", 0.56))
                .max(pa * crate::env_f("BLDG_MIN", 0.24));
            // The footprint is an oriented rectangle aligned to the parcel's
            // long axis - which, after strip subdivision, is the direction the
            // plot faces the street. Taking the inset parcel polygon itself
            // (PRD 7.2 step 5, read literally) puts triangles and wedges on the
            // map wherever subdivision produced a triangular plot, and a
            // triangular building does not read as a building.
            let axis = longest_axis(parcel);
            let across = perp(axis);
            let (la0, la1) = extent_along(parcel, axis);
            let (lb0, lb1) = extent_along(parcel, across);
            let ratio = ((la1 - la0) / (lb1 - lb0).max(1e-9)).clamp(1.0, 3.4);
            let half_a = (want * ratio).sqrt() * 0.5;
            let half_b = (want / ratio).sqrt() * 0.5;
            let mut rng = SeededRng::for_path(&f.path, "building.rotation");
            let ang = (rng.next_f64() - 0.5) * 2.0 * (4.0 * std::f64::consts::PI / 180.0);
            let (sn, cs) = det_sin_cos(ang);
            let mut k2 = 1.0f64;
            let mut ring = Vec::new();
            let mut ok = false;
            for _ in 0..20 {
                let ha = half_a * k2;
                let hb = half_b * k2;
                let rect: Vec<P> = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)]
                    .iter()
                    .map(|(u, w)| {
                        [
                            anchor[0] + axis[0] * ha * u + across[0] * hb * w,
                            anchor[1] + axis[1] * ha * u + across[1] * hb * w,
                        ]
                    })
                    .collect();
                ring = rotate_about(&rect, anchor, sn, cs);
                ok = ring.iter().all(|p| {
                    contains(&lot_ring, *p) && dist_to_boundary(&blk_ring, *p) >= road_half
                });
                if ok {
                    break;
                }
                k2 *= 0.88;
            }
            if !ok {
                // Clamp against the *lot* as well: a sub-parcel of a non-convex
                // lot can bulge very slightly outside it.
                let r = (clearance.min(dist_to_boundary(&lot_ring, anchor)).max(1e-4) * 0.70)
                    .min((want * 0.5).sqrt());
                ring = rotate_about(
                    &[
                        [anchor[0] - r, anchor[1] - r],
                        [anchor[0] + r, anchor[1] - r],
                        [anchor[0] + r, anchor[1] + r],
                        [anchor[0] - r, anchor[1] + r],
                    ],
                    anchor,
                    sn,
                    cs,
                );
            }
            let height = if f.monument {
                2.4
            } else if f.industrial {
                0.7
            } else {
                0.6 + ((f.size_bytes.max(1) as f64).ln() / 11.0).clamp(0.0, 1.2)
            };
            buildings.push(Building {
                ring,
                file: *fi,
                lot: u32::try_from(li).expect("fits"),
                monument: f.monument,
                industrial: f.industrial,
                height,
                fallback: !ok,
            });
        }
    }

    // --- district boundaries ---------------------------------------------
    let mut edge_side: Vec<[i64; 2]> = vec![[-1, -1]; g.edges.len()];
    for (bi, b) in blocks.iter().enumerate() {
        let d = match b.district {
            Some(d) => i64::from(d),
            None => -2,
        };
        let _ = bi;
        for &h in &b.half_edges {
            let e = h / 2;
            let slot = h % 2;
            edge_side[e][slot] = d;
        }
    }
    // Top-level package of each district, for the coarse boundary layer.
    let top_of: Vec<u32> = s
        .districts
        .iter()
        .map(|d| {
            let t = d.path.as_str().split('/').next().unwrap_or("");
            s.districts
                .iter()
                .position(|x| x.path.as_str() == t)
                .unwrap_or(0) as u32
        })
        .collect();
    let mut package_edges = Vec::new();
    let mut district_edges = Vec::new();
    for (e, s2) in edge_side.iter().enumerate() {
        let a = s2[0];
        let b = s2[1];
        if a != b && a != -2 && b != -2 && a >= 0 && b >= 0 {
            district_edges.push(e);
            if top_of[a as usize] != top_of[b as usize] {
                package_edges.push(e);
            }
        } else if (a >= 0) != (b >= 0) && (a == -1 || b == -1) {
            district_edges.push(e);
            package_edges.push(e);
        }
    }

    // --- streets: cross-district imports routed along the roads (PRD §9) ---
    let mut district_of_file: Vec<u32> = vec![0; s.files.len()];
    for (i, f) in s.files.iter().enumerate() {
        let dp = f.path.parent().unwrap_or_else(LogicalPath::root);
        district_of_file[i] = *s.district_index.get(&dp).unwrap_or(&0);
    }
    let mut pair_weight: BTreeMap<(u32, u32), u32> = BTreeMap::new();
    for &(a, b) in imports {
        let da = district_of_file[a as usize];
        let db = district_of_file[b as usize];
        if da != db {
            *pair_weight.entry((da.min(db), da.max(db))).or_insert(0) += 1;
        }
    }
    let streets = route_streets(s, g, &pair_weight);

    City {
        blocks,
        lots,
        buildings,
        streets,
        district_edges,
        package_edges,
        district_of_file,
        civic_block: civic,
    }
}

fn nearest_node(g: &Graph, p: P) -> Option<u32> {
    let mut best: Option<(f64, u32)> = None;
    for (i, n) in g.nodes.iter().enumerate() {
        let d = dist2(*n, p);
        if best.is_none_or(|(b, _)| d < b) {
            best = Some((d, i as u32));
        }
    }
    best.map(|(_, i)| i)
}

fn route_streets(
    s: &Settlement,
    g: &Graph,
    pairs: &BTreeMap<(u32, u32), u32>,
) -> Vec<Street> {
    if g.nodes.is_empty() || pairs.is_empty() {
        return Vec::new();
    }
    // Top pairs by weight, deterministically tie-broken by district ids.
    let mut ranked: Vec<((u32, u32), u32)> = pairs.iter().map(|(k, v)| (*k, *v)).collect();
    ranked.sort_by_key(|(k, v)| (std::cmp::Reverse(*v), *k));
    ranked.truncate(160);

    // Node nearest each involved district's centre of mass.
    let mut anchor: BTreeMap<u32, u32> = BTreeMap::new();
    for ((a, b), _) in &ranked {
        for d in [*a, *b] {
            if anchor.contains_key(&d) {
                continue;
            }
            let ds = &s.districts[d as usize];
            if ds.plots.is_empty() {
                continue;
            }
            if let Some(n) = nearest_node(g, ds.centroid()) {
                anchor.insert(d, n);
            }
        }
    }
    let mut by_src: BTreeMap<u32, Vec<(u32, u32)>> = BTreeMap::new();
    for ((a, b), w) in &ranked {
        by_src.entry(*a).or_default().push((*b, *w));
    }

    let n = g.nodes.len();
    let mut out = Vec::new();
    let mut sources: Vec<u32> = by_src.keys().copied().collect();
    sources.truncate(80);
    for src in sources {
        let Some(&s0) = anchor.get(&src) else { continue };
        // Dijkstra with a sorted frontier keyed on quantised cost then node id.
        let mut distv = vec![f64::INFINITY; n];
        let mut prev = vec![u32::MAX; n];
        let mut heap: std::collections::BinaryHeap<(std::cmp::Reverse<i64>, std::cmp::Reverse<u32>)> =
            std::collections::BinaryHeap::new();
        distv[s0 as usize] = 0.0;
        heap.push((std::cmp::Reverse(0), std::cmp::Reverse(s0)));
        while let Some((std::cmp::Reverse(dq), std::cmp::Reverse(u))) = heap.pop() {
            let du = dq as f64 * 1e-5;
            if du > distv[u as usize] + 1e-9 {
                continue;
            }
            for &e in &g.adj[u as usize] {
                let v = g.other(e as usize, u);
                // Arterials are cheaper: a street prefers a main road, exactly
                // as traffic does.
                let mult = match g.class[e as usize] {
                    RoadClass::Arterial => 0.72,
                    RoadClass::Street => 0.88,
                    RoadClass::Lane => 1.0,
                };
                let nd = du + g.edge_len(e as usize) * mult;
                if nd + 1e-9 < distv[v as usize] {
                    distv[v as usize] = nd;
                    prev[v as usize] = e;
                    heap.push((std::cmp::Reverse((nd * 1e5) as i64), std::cmp::Reverse(v)));
                }
            }
        }
        for (dst, w) in by_src.get(&src).into_iter().flatten() {
            let Some(&t0) = anchor.get(dst) else { continue };
            if !distv[t0 as usize].is_finite() {
                continue;
            }
            let mut poly: Vec<P> = Vec::new();
            let mut cur = t0;
            let mut guard = 0;
            while cur != s0 && prev[cur as usize] != u32::MAX && guard < n {
                guard += 1;
                let e = prev[cur as usize] as usize;
                let nxt = g.other(e, cur);
                let c = g.directed_curve(e, cur);
                for p in c.iter().take(c.len() - 1) {
                    poly.push(*p);
                }
                cur = nxt;
            }
            poly.push(g.nodes[s0 as usize]);
            if poly.len() >= 2 {
                out.push(Street {
                    polyline: poly,
                    weight: *w,
                });
            }
        }
    }
    out
}
