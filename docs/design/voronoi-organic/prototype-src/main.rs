//! voronoi-organic — prototype driver.
//!
//! Usage: voronoi-organic <out-dir> [small|large|both] [--permute] [--no-png]

mod city;
mod font;
mod geom;
mod metrics;
mod png;
mod raster;
mod render;
mod synth;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use polis_layout::determinism as det;

use city::{City, FileRec, Repo};

fn real_repo(root: &Path) -> Repo {
    use polis_repo::tree::{RepoIndex, WalkOptions};
    let mut opts = WalkOptions::default();
    opts.skip_massed = true;
    let mut idx = RepoIndex::open_with(root, opts).expect("index");
    if let Ok(seq) = polis_repo::git::GrowthSequence::bootstrap(root) {
        idx.apply_growth(&seq.entries);
        idx.set_head(seq.head.clone());
    }
    let tree = idx.tree().clone();
    let ig = polis_repo::imports::ImportGraph::build(&tree);
    let streets: Vec<(String, String, u32)> = ig
        .cross_district_edges()
        .into_iter()
        .map(|s| {
            (
                s.from.as_str().to_string(),
                s.to.as_str().to_string(),
                s.edge_count,
            )
        })
        .collect();
    let files: Vec<FileRec> = tree
        .files
        .values()
        .filter(|f| {
            let p = f.path.as_str();
            // The competing design prototypes write here while this runs; their
            // churn is not part of the repository being drawn.
            !p.starts_with("target/") && !p.starts_with("docs/design/")
        })
        .map(|f| FileRec {
            path: f.path.as_str().to_string(),
            size: f.size_bytes,
            growth: f.growth_index,
        })
        .collect();
    Repo { files, streets }
}

/// Canonical, quantised serialisation of everything the layout produces.
/// This is what the determinism gate hashes.
fn dump(c: &City) -> String {
    let mut s = String::with_capacity(1 << 20);
    let _ = writeln!(s, "schema voronoi-organic/1");
    let _ = writeln!(
        s,
        "k_claim {:.3} iters {}",
        det::quantize_f64(c.k_claim),
        c.k_iters
    );
    let _ = writeln!(s, "districts {}", c.districts.len());
    for d in &c.districts {
        let _ = writeln!(
            s,
            "D {} px={} cx={:.3} cy={:.3} age={:.6} blocks={}",
            d.key,
            d.px,
            det::quantize_f64(d.centroid.x),
            det::quantize_f64(d.centroid.y),
            d.age_rank,
            d.blocks.len()
        );
    }
    let _ = writeln!(s, "chains {}", c.chains.iter().filter(|x| x.alive).count());
    for (i, ch) in c.chains.iter().enumerate() {
        if !ch.alive {
            continue;
        }
        let _ = write!(s, "C {i} {} {} {:?}", ch.a, ch.b, ch.class);
        for p in &ch.pts {
            let _ = write!(s, " {:.3},{:.3}", det::quantize_f64(p.x), det::quantize_f64(p.y));
        }
        let _ = writeln!(s);
    }
    let _ = writeln!(s, "blocks {}", c.blocks.len());
    for (i, b) in c.blocks.iter().enumerate() {
        let _ = write!(s, "B {i} d={} r={} cap={} n={}", b.district, b.rank, b.cap, b.files.len());
        for p in &b.poly {
            let _ = write!(s, " {:.3},{:.3}", det::quantize_f64(p.x), det::quantize_f64(p.y));
        }
        let _ = writeln!(s);
        for (li, lot) in b.lots.iter().enumerate() {
            let _ = write!(s, "L {i}.{li}");
            for p in lot {
                let _ = write!(s, " {:.3},{:.3}", det::quantize_f64(p.x), det::quantize_f64(p.y));
            }
            let _ = writeln!(s);
        }
    }
    let mut bs: Vec<&city::Building> = c.buildings.iter().collect();
    bs.sort_by(|a, b| {
        c.repo.files[a.file as usize]
            .path
            .cmp(&c.repo.files[b.file as usize].path)
    });
    let _ = writeln!(s, "buildings {}", bs.len());
    for b in bs {
        let _ = write!(s, "U {}", c.repo.files[b.file as usize].path);
        for p in &b.poly {
            let _ = write!(s, " {:.3},{:.3}", det::quantize_f64(p.x), det::quantize_f64(p.y));
        }
        let _ = writeln!(s);
    }
    let _ = writeln!(s, "streets {}", c.street_paths.len());
    for (path, n) in &c.street_paths {
        let _ = write!(s, "S {n} {}", path.len());
        for p in path {
            let _ = write!(s, " {:.3},{:.3}", det::quantize_f64(p.x), det::quantize_f64(p.y));
        }
        let _ = writeln!(s);
    }
    s
}

/// One growth step: add a file and re-lay only the block it lands in.
fn incremental_add(c: &mut City, path: &str, size: u64) -> (f64, String) {
    let t = Instant::now();
    let dir = city::dir_of(path).to_string();
    let key = if dir.is_empty() {
        "/·".to_string()
    } else {
        format!("{dir}/·")
    };
    let Some(di) = c.districts.iter().position(|d| d.key == key) else {
        return (
            t.elapsed().as_secs_f64() * 1e6,
            "new directory: needs a district-level step".into(),
        );
    };
    // Would the quantised weight of this directory or any ancestor change?
    let mut n = c.districts[di].node;
    let mut resize = false;
    loop {
        let cnt = c.nodes[n as usize].count;
        if city::quantise_count(cnt) != city::quantise_count(cnt + 1) {
            resize = true;
        }
        let p = c.nodes[n as usize].parent;
        if p == city::NOLABEL {
            break;
        }
        n = p;
    }
    let fi = u32::try_from(c.repo.files.len()).unwrap();
    let growth = c.repo.files.iter().map(|f| f.growth).max().unwrap_or(0) + 1;
    c.repo.files.push(FileRec {
        path: path.to_string(),
        size,
        growth,
    });
    c.districts[di].files.push(fi);
    let ids = c.districts[di].blocks.clone();
    let target = ids
        .iter()
        .copied()
        .find(|&b| (c.blocks[b as usize].files.len() as u32) < c.blocks[b as usize].cap)
        .unwrap_or(ids[ids.len() - 1]);
    c.blocks[target as usize].files.push(fi);
    c.rebuild_block(target as usize, 0.9);
    let us = t.elapsed().as_secs_f64() * 1e6;
    (
        us,
        format!(
            "block {target} in district {key}; district resize needed: {resize}"
        ),
    )
}

fn write_png(path: &PathBuf, w: usize, h: usize, rgb: &[u8]) {
    std::fs::write(path, png::encode_rgb(w, h, rgb)).expect("write png");
}

fn run(
    name: &str,
    repo: Repo,
    outdir: &Path,
    prefix: &str,
    want_png: bool,
) -> (String, String) {
    let nfiles = repo.files.len();
    let t = Instant::now();
    let mut c = city::generate(repo);
    let gen_ms = t.elapsed().as_secs_f64() * 1e3;

    let before = dump(&c);

    // Incremental step: pick an existing district and drop a file into it.
    // Prefer a district where the quantised weight absorbs the new file, which
    // is the case the incremental promise is actually about.
    let absorbs = |c: &City, di: usize| -> bool {
        let mut n = c.districts[di].node;
        loop {
            let cnt = c.nodes[n as usize].count;
            if city::quantise_count(cnt) != city::quantise_count(cnt + 1) {
                return false;
            }
            let p = c.nodes[n as usize].parent;
            if p == city::NOLABEL {
                return true;
            }
            n = p;
        }
    };
    let pick = (0..c.districts.len())
        .filter(|&i| absorbs(&c, i))
        .max_by_key(|&i| c.districts[i].px)
        .or_else(|| (0..c.districts.len()).max_by_key(|&i| c.districts[i].px));
    let victim = pick
        .map(|i| c.districts[i].key.trim_end_matches("/·").to_string())
        .unwrap_or_default();
    let newpath = if victim.is_empty() {
        "zz_new_file.rs".to_string()
    } else {
        format!("{victim}/zz_new_file.rs")
    };
    let mut c2 = City {
        repo: c.repo.clone(),
        nodes: c.nodes.clone(),
        districts: c.districts.clone(),
        blocks: c.blocks.clone(),
        buildings: c.buildings.clone(),
        chains: c.chains.clone(),
        jpos: c.jpos.clone(),
        street_paths: c.street_paths.clone(),
        land: Vec::new(),
        dlabel: Vec::new(),
        blabel: Vec::new(),
        k_claim: c.k_claim,
        k_iters: c.k_iters,
        size_median: c.size_median,
        notes: c.notes.clone(),
    };
    let (inc_us, inc_note) = incremental_add(&mut c2, &newpath, 4096);
    let after = dump(&c2);
    // How much of the world moved?
    // Honest "how much of the world moved": lines present before and gone after.
    let post: std::collections::BTreeSet<&str> = after.lines().collect();
    let moved = before.lines().filter(|l| !post.contains(l)).count();
    let stable = before.lines().count();

    let m = metrics::measure(&c, gen_ms, inc_us, name);
    let mut lines = m.lines.clone();
    lines.push(format!(
        "INCREMENTAL  {inc_note}; {moved} of {stable} dump lines changed"
    ));

    let text = lines.join("\n");
    println!("{text}\n");

    if want_png {
        let info: Vec<String> = vec![
            lines[1].clone(),
            lines[2].clone(),
            lines[3].clone(),
            lines[4].clone(),
            lines[6].clone(),
            lines[7].clone(),
            lines[8].clone(),
            lines[9].clone(),
        ];
        let title = format!(
            "VORONOI-ORGANIC / {} / {nfiles} FILES",
            name.to_uppercase()
        );
        let (w, h, rgb) = render::render_city(&c, &title, &info);
        // legend drawn into the info strip
        let mut cv = raster::Canvas::new(w, h, [0, 0, 0]);
        let _ = &mut cv;
        write_png(&outdir.join(format!("{prefix}.png")), w, h, &rgb);

        let jinfo: Vec<String> = vec![
            lines[3].clone(),
            lines[4].clone(),
            lines[5].clone(),
            lines[6].clone(),
            "DOT COLOUR = JUNCTION DEGREE:  MAGENTA 1 (DANGLING)  BLUE 2  CYAN 3  ORANGE 4  RED 5+"
                .to_string(),
            "A TREE WOULD BE ALL CYAN AND MAGENTA WITH ZERO CYCLES.".to_string(),
        ];
        let (w, h, rgb) = render::render_junctions(
            &c,
            &format!("ROAD GRAPH ONLY / {} / JUNCTIONS BY DEGREE", name.to_uppercase()),
            &jinfo,
        );
        let jname = if prefix == "small" {
            "junctions.png".to_string()
        } else {
            format!("junctions-{prefix}.png")
        };
        write_png(&outdir.join(jname), w, h, &rgb);
    }

    (text, before)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let outdir = PathBuf::from(
        args.get(1)
            .cloned()
            .unwrap_or_else(|| ".".to_string()),
    );
    let mode = args.get(2).cloned().unwrap_or_else(|| "both".to_string());
    let permute = args.iter().any(|a| a == "--permute");
    let want_png = !args.iter().any(|a| a == "--no-png");
    std::fs::create_dir_all(&outdir).ok();

    let mut all = String::new();

    if mode == "small" || mode == "both" {
        let mut r = real_repo(Path::new("C:/coding/agentolis"));
        if permute {
            // Reverse the input order: the layout must not notice.
            r.files.reverse();
            r.streets.reverse();
        }
        let (t, d) = run("agentolis (real repo)", r, &outdir, "small", want_png);
        all.push_str(&t);
        all.push('\n');
        std::fs::write(outdir.join("dump-small.txt"), d).ok();
    }
    if mode == "large" || mode == "both" {
        let mut r = synth::synth();
        if permute {
            r.files.reverse();
            r.streets.reverse();
        }
        let (t, d) = run("synthetic 5k", r, &outdir, "large", want_png);
        all.push_str(&t);
        all.push('\n');
        std::fs::write(outdir.join("dump-large.txt"), d).ok();
    }
    std::fs::write(outdir.join("metrics.txt"), all).ok();
}
