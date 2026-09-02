mod arr;
mod city;
mod geom;
mod metrics;
mod render;
mod synth;

use std::path::Path;
use std::time::Instant;

use polis_events::LogicalPath;
use polis_layout::determinism::fnv1a64;
use polis_repo::{FileMeta, RepoTree};

use arr::Class;
use city::{Layout, Params};
use geom::{pt, Pt};
use render::{hsv, mix, Canvas, Rgb};

const BG: Rgb = [0.043, 0.051, 0.067];
const PANEL: Rgb = [0.078, 0.090, 0.114];
const INK: Rgb = [0.80, 0.84, 0.90];
const DIM: Rgb = [0.46, 0.50, 0.58];

fn district_hue(top: u32) -> f64 {
    if top == u32::MAX {
        return 0.12;
    }
    (0.075 + f64::from(top) * 0.3819660112501051) % 1.0
}

const ASPHALT: Rgb = [0.145, 0.160, 0.190];

fn road_paint(c: Class, border: bool) -> Rgb {
    match (c, border) {
        (Class::Rim, _) => [0.20, 0.21, 0.24],
        (Class::Boulevard, _) => [0.36, 0.31, 0.25],
        (Class::Avenue, _) => [0.30, 0.28, 0.24],
        (_, true) => [0.26, 0.25, 0.23],
        (Class::Street, false) => ASPHALT,
    }
}

fn layout_bounds(l: &Layout) -> (Pt, Pt) {
    let mut lo = pt(f64::MAX, f64::MAX);
    let mut hi = pt(f64::MIN, f64::MIN);
    for p in &l.nodes {
        lo.x = lo.x.min(p.x);
        lo.y = lo.y.min(p.y);
        hi.x = hi.x.max(p.x);
        hi.y = hi.y.max(p.y);
    }
    (lo, hi)
}

fn paint(c: &mut Canvas, l: &Layout, params: &Params, big: bool) {
    // 1. district ground: every block filled with its district's hue
    for b in &l.blocks {
        let d = &l.districts[b.district as usize];
        let base = hsv(district_hue(d.top), 0.50, 0.34);
        let ground = mix(BG, base, 0.40 + 0.30 * d.oldness);
        c.fill(&b.poly, ground, 1.0);
    }
    // 2. roads at their true ground width; widest first so the narrow streets
    // sit on top at a junction.
    let hw = params.road_half_width;
    let mut order: Vec<usize> = (0..l.segs.len()).collect();
    order.sort_by_key(|&i| l.segs[i].2 as u8);
    for &i in &order {
        let (a, b, cl) = l.segs[i];
        c.polyline_world(
            &[l.nodes[a as usize], l.nodes[b as usize]],
            hw[cl as usize] * 2.0,
            road_paint(cl, l.seg_is_border[i]),
            1.0,
            0.7,
        );
    }
    // 3. courtyards, then lots
    for g in &l.gardens {
        c.fill(g, [0.105, 0.135, 0.108], 1.0);
    }
    for lot in &l.lots {
        let col = if lot.occupant.is_some() {
            [0.120, 0.135, 0.160]
        } else {
            [0.100, 0.150, 0.112]
        };
        c.fill(&lot.poly, col, 0.92);
        if !big {
            c.polyline(&lot.poly, 0.5, [0.30, 0.34, 0.40], 0.5, true);
        }
    }
    // 4. buildings
    for b in &l.buildings {
        if b.poly.len() < 3 {
            continue;
        }
        let t = (b.height / 46.0).clamp(0.0, 1.0);
        let col = if b.monument {
            [1.0, 0.84, 0.36]
        } else {
            mix([0.44, 0.49, 0.57], [0.92, 0.91, 0.87], t)
        };
        c.fill(&b.poly, col, 1.0);
        if !big {
            c.polyline(&b.poly, 0.4, [0.07, 0.09, 0.12], 0.6, true);
        }
    }
    // 5. district borders: a bright centre line on the arterial they already are
    for (i, &(a, b, cl)) in l.segs.iter().enumerate() {
        if !l.seg_is_border[i] || cl == Class::Street {
            continue;
        }
        let w = if cl == Class::Boulevard { 1.5 } else { 1.0 };
        c.polyline(
            &[l.nodes[a as usize], l.nodes[b as usize]],
            if big { w * 0.7 } else { w },
            [0.98, 0.76, 0.42],
            0.85,
            false,
        );
    }
    // 6. streets: cross-district imports, routed along the network
    for s in &l.streets {
        let wd = (1.2 + f64::from(s.weight) * 0.20).min(4.0);
        c.polyline(
            &s.poly,
            if big { wd * 0.7 } else { wd },
            [0.22, 0.88, 0.88],
            0.60,
            false,
        );
    }
}

fn draw_city(l: &Layout, params: &Params, w: usize, h: usize, title: &str, sub: &str) -> Canvas {
    let mut c = Canvas::new(w, h, 2, BG);
    let (lo, hi) = layout_bounds(l);
    c.fit(lo, hi, 46.0);
    let big = l.buildings.len() > 1200;
    paint(&mut c, l, params, big);
    legend(&mut c, l, title, sub, big);
    c
}

/// A zoomed crop, so the fabric can be judged rather than guessed at.
fn draw_detail(l: &Layout, params: &Params, dim: usize, frac: f64, title: &str) -> Canvas {
    let mut c = Canvas::new(dim, dim, 2, BG);
    let (lo, hi) = layout_bounds(l);
    let cx = (lo.x + hi.x) * 0.5;
    let cy = (lo.y + hi.y) * 0.5;
    let r = (hi.x - lo.x).max(hi.y - lo.y) * frac * 0.5;
    c.fit(pt(cx - r, cy - r), pt(cx + r, cy + r), 8.0);
    paint(&mut c, l, params, false);
    c.text(20.0, 16.0, title, 2.6, INK, 1.0);
    c
}

fn legend(c: &mut Canvas, l: &Layout, title: &str, sub: &str, big: bool) {
    let w = c.w as f64;
    let h = c.h as f64;
    c.text(26.0, 22.0, title, 3.4, INK, 1.0);
    c.text(26.0, 22.0 + 26.0, sub, 2.0, DIM, 1.0);

    let items: [(&str, Rgb, u8); 10] = [
        ("BOULEVARD / AVENUE = DISTRICT BORDER", [0.98, 0.76, 0.42], 0),
        ("STREET (INSIDE A DISTRICT)", ASPHALT, 0),
        ("STREET LAYER: CROSS-DISTRICT IMPORTS", [0.20, 0.86, 0.86], 0),
        ("BLOCK (CLOSED FACE OF THE ROAD GRAPH)", [0.24, 0.30, 0.38], 1),
        ("LOT (OCCUPIED)", [0.115, 0.130, 0.155], 2),
        ("LOT (VACANT)", [0.105, 0.150, 0.115], 2),
        ("BLOCK COURTYARD / GARDEN", [0.105, 0.135, 0.108], 1),
        ("BUILDING (BRIGHTER = BIGGER FILE)", [0.90, 0.91, 0.90], 1),
        ("MONUMENT (ENTRY POINT)", [1.0, 0.86, 0.42], 1),
        ("DISTRICT GROUND (HUE = TOP-LEVEL DIR)", [0.30, 0.35, 0.45], 1),
    ];
    let pad = 10.0;
    let lh = 15.0;
    let bw = 330.0;
    let bh = pad * 2.0 + lh * items.len() as f64;
    let x0 = 22.0;
    let y0 = h - bh - 22.0;
    c.fill_px(
        &[
            pt(x0, y0),
            pt(x0 + bw, y0),
            pt(x0 + bw, y0 + bh),
            pt(x0, y0 + bh),
        ],
        PANEL,
        0.90,
    );
    for (i, (name, col, kind)) in items.iter().enumerate() {
        let y = y0 + pad + i as f64 * lh;
        match kind {
            0 => c.line_px(pt(x0 + 8.0, y + 4.0), pt(x0 + 26.0, y + 4.0), 3.0, *col, 1.0),
            _ => c.fill_px(
                &[
                    pt(x0 + 8.0, y),
                    pt(x0 + 26.0, y),
                    pt(x0 + 26.0, y + 8.0),
                    pt(x0 + 8.0, y + 8.0),
                ],
                *col,
                1.0,
            ),
        }
        c.text(x0 + 32.0, y + 1.0, name, 1.35, INK, 0.95);
    }

    // right-hand stats
    let stats = [
        format!("FILES {}", l.files.len()),
        format!("DISTRICTS {}", l.districts.len()),
        format!("BLOCKS {}", l.blocks.len()),
        format!("LOTS {}", l.lots.len()),
        format!("ROAD NODES {}", l.nodes.len()),
        format!("ROAD SEGS {}", l.segs.len()),
    ];
    let sw = 150.0;
    let sh = pad * 2.0 + lh * stats.len() as f64;
    let sx = w - sw - 22.0;
    let sy = h - sh - 22.0;
    c.fill_px(
        &[
            pt(sx, sy),
            pt(sx + sw, sy),
            pt(sx + sw, sy + sh),
            pt(sx, sy + sh),
        ],
        PANEL,
        0.90,
    );
    for (i, s) in stats.iter().enumerate() {
        c.text(sx + 10.0, sy + pad + i as f64 * lh, s, 1.5, INK, 0.95);
    }
    let _ = big;
}

fn draw_junctions(l: &Layout, w: usize, h: usize, title: &str, sub: &str) -> Canvas {
    let ss = 2;
    let mut c = Canvas::new(w, h, ss, [0.030, 0.035, 0.047]);
    let (lo, hi) = layout_bounds(l);
    c.fit(lo, hi, 46.0);
    let big = l.segs.len() > 6000;
    let mut deg = vec![0u32; l.nodes.len()];
    for &(a, b, _) in &l.segs {
        deg[a as usize] += 1;
        deg[b as usize] += 1;
    }
    for &(a, b, cl) in &l.segs {
        let wd = match cl {
            Class::Street => 0.55,
            _ => 1.1,
        };
        c.polyline(
            &[l.nodes[a as usize], l.nodes[b as usize]],
            if big { wd * 0.7 } else { wd },
            [0.30, 0.34, 0.42],
            0.95,
            false,
        );
    }
    let style = |d: u32| -> (Rgb, f64) {
        match d {
            0 => ([0.0, 0.0, 0.0], 0.0),
            1 => ([1.00, 0.24, 0.28], 3.0),
            2 => ([0.30, 0.34, 0.42], 0.7),
            3 => ([0.32, 0.60, 1.00], 1.5),
            4 => ([0.28, 0.95, 0.48], 2.4),
            _ => ([1.00, 0.84, 0.20], 3.0),
        }
    };
    let mut counts = [0usize; 6];
    for (i, &d) in deg.iter().enumerate() {
        if d == 0 {
            continue;
        }
        counts[(d as usize).min(5)] += 1;
        let (col, r) = style(d);
        if r <= 0.0 {
            continue;
        }
        c.disc(l.nodes[i], if big { r * 0.62 } else { r }, col, 0.95);
    }
    c.text(26.0, 22.0, title, 3.4, INK, 1.0);
    c.text(26.0, 48.0, sub, 2.0, DIM, 1.0);

    let names = [
        ("DEG 1  DANGLING", 1usize),
        ("DEG 2  BEND", 2),
        ("DEG 3  T-JUNCTION", 3),
        ("DEG 4  CROSSROAD", 4),
        ("DEG 5+ IRREGULAR", 5),
    ];
    let pad = 10.0;
    let lh = 16.0;
    let bw = 250.0;
    let bh = pad * 2.0 + lh * (names.len() + 2) as f64;
    let x0 = 22.0;
    let y0 = c.h as f64 - bh - 22.0;
    c.fill_px(
        &[
            pt(x0, y0),
            pt(x0 + bw, y0),
            pt(x0 + bw, y0 + bh),
            pt(x0, y0 + bh),
        ],
        PANEL,
        0.92,
    );
    for (i, (n, d)) in names.iter().enumerate() {
        let y = y0 + pad + i as f64 * lh;
        let (col, _) = style(*d as u32);
        c.fill_px(
            &[
                pt(x0 + 9.0, y),
                pt(x0 + 21.0, y),
                pt(x0 + 21.0, y + 9.0),
                pt(x0 + 9.0, y + 9.0),
            ],
            col,
            1.0,
        );
        c.text(
            x0 + 28.0,
            y + 1.0,
            &format!("{n}   {}", counts[*d]),
            1.45,
            INK,
            0.95,
        );
    }
    let v = deg.iter().filter(|&&d| d > 0).count() as i64;
    let cyc = l.segs.len() as i64 - v + 1;
    c.text(
        x0 + 9.0,
        y0 + pad + (names.len() as f64 + 0.4) * lh,
        &format!("CYCLES E-V+C = {cyc}"),
        1.45,
        [0.55, 0.95, 0.65],
        1.0,
    );
    c.text(
        x0 + 9.0,
        y0 + pad + (names.len() as f64 + 1.4) * lh,
        "A TREE WOULD HAVE 0 CYCLES",
        1.45,
        DIM,
        1.0,
    );
    c
}

fn load_real_repo(root: &str) -> anyhow::Result<(RepoTree, Vec<(LogicalPath, LogicalPath)>)> {
    use polis_repo::tree::{RepoIndex, WalkOptions};
    let opts = WalkOptions {
        skip_massed: true,
        follow_symlinks: false,
        max_files: Some(20_000),
        industrial_rules: None,
    };
    let mut index = RepoIndex::open_with(Path::new(root), opts)?;
    if let Ok(seq) = polis_repo::git::GrowthSequence::bootstrap(Path::new(root)) {
        index.apply_growth(&seq.entries);
    }
    let mut tree = index.tree().clone();
    // This prototype writes its own renders into docs/design/, and the walk would
    // otherwise pick them up and grow the city on every run. Generated artefacts
    // are not part of the repository's shape.
    let generated = LogicalPath::new("docs/design").unwrap();
    tree.files.retain(|p, _| !p.starts_with(&generated));
    let graph = polis_repo::imports::ImportGraph::build(&tree);
    let mut edges = Vec::new();
    for e in graph.internal_edges() {
        if let polis_repo::ImportTarget::Internal(to) = &e.to {
            edges.push((e.from.clone(), to.clone()));
        }
    }
    Ok((tree, edges))
}

struct Run {
    name: String,
    layout: Layout,
    m: metrics::Metrics,
    gen_ms: f64,
    inc_us: f64,
    inc_resub_ms: f64,
}

fn generate(
    name: &str,
    tree: &RepoTree,
    imports: &[(LogicalPath, LogicalPath)],
    params: &Params,
) -> Run {
    let t0 = Instant::now();
    let mut layout = city::build(tree, params);
    let pairs = city::street_pairs(&layout, imports, 70);
    city::route_streets(&mut layout, &pairs);
    let gen_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // incremental: one file added into a district that has a spare lot
    let target = layout
        .lots_of_district
        .iter()
        .find(|(_, ids)| ids.iter().any(|&l| layout.lots[l as usize].occupant.is_none()))
        .map(|(&d, _)| d);
    let mut inc_us = 0.0;
    if let Some(d) = target {
        let dir = layout.districts[d as usize].path.clone();
        let p = if dir.is_root() {
            LogicalPath::new("zz_new_file.rs").unwrap()
        } else {
            dir.join("zz_new_file.rs").unwrap()
        };
        let meta = FileMeta::untracked(p, 4096);
        let t = Instant::now();
        let _ = city::add_file(&mut layout, &meta, params);
        inc_us = t.elapsed().as_secs_f64() * 1e6;
    }
    // incremental worst case: the quantised weight moved, so one district's
    // subtree is re-subdivided. Nothing outside its polygon can move.
    let biggest = (0..layout.districts.len())
        .max_by_key(|&i| layout.districts[i].direct_files)
        .unwrap_or(0) as u32;
    let t = Instant::now();
    city::resubdivide_district(&layout, biggest, params);
    let inc_resub_ms = t.elapsed().as_secs_f64() * 1000.0;

    let ring_ok = city::RING_OK.swap(0, std::sync::atomic::Ordering::Relaxed);
    let ring_fb = city::RING_FALLBACK.swap(0, std::sync::atomic::Ordering::Relaxed);
    let no_ground = city::BLOCK_NO_GROUND.swap(0, std::sync::atomic::Ordering::Relaxed);
    let unsplit = city::UNSPLITTABLE.swap(0, std::sync::atomic::Ordering::Relaxed);
    eprintln!(
        "   [{name}] perimeter-lot blocks={ring_ok} chord-split fallback={ring_fb} blocks with no buildable ground={no_ground} unsplittable faces={unsplit}"
    );
    let m = metrics::measure(&layout, &params.road_half_width);
    Run {
        name: name.to_string(),
        layout,
        m,
        gen_ms,
        inc_us,
        inc_resub_ms,
    }
}

fn hash_file(p: &str) -> String {
    match std::fs::read(p) {
        Ok(b) => format!("{:016x}", fnv1a64(&b)),
        Err(_) => "missing".into(),
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let outdir = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "C:/coding/agentolis/docs/design/treemap-arterials".to_string());
    std::fs::create_dir_all(&outdir)?;

    let mut report = String::new();

    // ---- small: the real repository ---------------------------------------
    let (small_tree, small_imports) = load_real_repo("C:/coding/agentolis")?;
    let envf = |k: &str, d: f64| -> f64 {
        std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
    };
    let small_params = Params {
        warp_amp: envf("WA", 0.22),
        warp_len: envf("WL", 8.0),
        ..Params::default()
    };
    let small = generate("small (real repo)", &small_tree, &small_imports, &small_params);

    // ---- large: 5000 synthetic files --------------------------------------
    let large_tree = synth::synth_repo(5000);
    let large_imports = synth::synth_imports(&large_tree);
    let large_params = Params {
        warp_amp: envf("WA", 0.22),
        warp_len: envf("WL", 8.0),
        arterial_bend: envf("AB", 0.045),
        ..Params::default()
    };
    let large = generate("large (5000 synthetic)", &large_tree, &large_imports, &large_params);

    for r in [&small, &large] {
        report.push_str(&metrics::report(
            &r.name,
            &r.m,
            r.gen_ms,
            r.inc_us,
            r.inc_resub_ms,
        ));
        report.push('\n');
    }

    // ---- renders -----------------------------------------------------------
    let dim = 1800usize;
    let c = draw_city(
        &small.layout,
        &small_params,
        dim,
        dim,
        "POLIS / TREEMAP-ARTERIALS",
        &format!(
            "AGENTOLIS REPO - {} FILES - DISTRICT BORDERS ARE THE ARTERIAL ROADS",
            small.layout.files.len()
        ),
    );
    render::write_png(&format!("{outdir}/small.png"), dim, dim, &c.to_rgb8())?;

    let c = draw_city(
        &large.layout,
        &large_params,
        dim,
        dim,
        "POLIS / TREEMAP-ARTERIALS",
        &format!(
            "SYNTHETIC MONOREPO - {} FILES - DISTRICT BORDERS ARE THE ARTERIAL ROADS",
            large.layout.files.len()
        ),
    );
    render::write_png(&format!("{outdir}/large.png"), dim, dim, &c.to_rgb8())?;

    let c = draw_detail(&large.layout, &large_params, dim, 0.16, "DETAIL: 16% OF THE 5000-FILE CITY");
    render::write_png(&format!("{outdir}/detail-large.png"), dim, dim, &c.to_rgb8())?;
    let c = draw_detail(&small.layout, &small_params, dim, 0.42, "DETAIL: 42% OF THE 99-FILE CITY");
    render::write_png(&format!("{outdir}/detail-small.png"), dim, dim, &c.to_rgb8())?;

    let c = draw_junctions(
        &small.layout,
        dim,
        dim,
        "ROAD GRAPH ONLY - JUNCTION DEGREE",
        "AGENTOLIS REPO. IF THIS WERE A TREE EVERY NODE WOULD BE RED OR BLUE AND CYCLES WOULD BE 0",
    );
    render::write_png(&format!("{outdir}/junctions.png"), dim, dim, &c.to_rgb8())?;

    let c = draw_junctions(
        &large.layout,
        dim,
        dim,
        "ROAD GRAPH ONLY - JUNCTION DEGREE",
        "SYNTHETIC 5000-FILE MONOREPO",
    );
    render::write_png(
        &format!("{outdir}/junctions-large.png"),
        dim,
        dim,
        &c.to_rgb8(),
    )?;

    // ---- determinism digests ----------------------------------------------
    let ds = city::digest(&small.layout);
    let dl = city::digest(&large.layout);
    std::fs::write(format!("{outdir}/digest-small.txt"), &ds)?;
    std::fs::write(format!("{outdir}/digest-large.txt"), &dl)?;
    // Input-ordering independence: rebuild both trees inserting the files in
    // reverse order and confirm the layout digest is unchanged. RepoTree is a
    // BTreeMap so this is structural, but PRD §7.4 asks for it to be shown.
    let mut shuffled_small = small_tree.clone();
    shuffled_small.files = small_tree.files.iter().rev().map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut shuffled_large = large_tree.clone();
    shuffled_large.files = large_tree.files.iter().rev().map(|(k, v)| (k.clone(), v.clone())).collect();
    let rs = city::digest(&city::build(&shuffled_small, &small_params));
    let rl = city::digest(&city::build(&shuffled_large, &large_params));

    report.push_str("=== determinism digests (FNV-1a 64 of the canonical layout dump) ===\n");
    report.push_str(&format!("layout small : {:016x}\n", fnv1a64(ds.as_bytes())));
    report.push_str(&format!("layout large : {:016x}\n", fnv1a64(dl.as_bytes())));
    let ds_no_streets = city::digest(&city::build(&small_tree, &small_params));
    let dl_no_streets = city::digest(&city::build(&large_tree, &large_params));
    report.push_str(&format!(
        "reversed input order small : {:016x}  (matches: {})
",
        fnv1a64(rs.as_bytes()),
        rs == ds_no_streets
    ));
    report.push_str(&format!(
        "reversed input order large : {:016x}  (matches: {})
",
        fnv1a64(rl.as_bytes()),
        rl == dl_no_streets
    ));
    for f in [
        "small.png",
        "large.png",
        "junctions.png",
        "junctions-large.png",
    ] {
        report.push_str(&format!(
            "png {f:20} : {}\n",
            hash_file(&format!("{outdir}/{f}"))
        ));
    }
    print!("{report}");
    std::fs::write(format!("{outdir}/metrics.txt"), &report)?;
    Ok(())
}
