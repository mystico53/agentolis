//! `polis snapshot` — a repository to a PNG, with no window (PRD §15 M1).
//!
//! > **M1 — Deterministic city, static.** `polis-repo` + `polis-layout`.
//! > Generate a city from git history and render it to a window (or PNG). No
//! > agents, no live data.
//!
//! This is the product path the M1 gate should be measured on. Before it existed
//! the gate had to write its own harness, which meant the thing being tested and
//! the thing being shipped were two different pieces of code.
//!
//! It also prints the structural metric set to stdout — connectivity, cycles,
//! junction degrees, block hierarchy, building coverage — because "does the map
//! look like a city" is a question with numbers behind it, and those numbers
//! belong next to the picture rather than in a notebook.
//!
//! Timings go to **stdout only**, never into the image: a wall-clock number
//! drawn on the PNG would make the image non-reproducible while the layout under
//! it was perfect, which is exactly the class of leak PRD §7.4 exists to stop.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use anyhow::Context as _;
use polis_events::WorktreeId;
use polis_layout::city::{self, City, LayoutInputs, Structure};
use polis_render::plan;
use polis_repo::git::History;
use polis_repo::tree::{walk_with, WalkExclusions, WalkOptions};
use polis_repo::{synthetic, RepoTree};

use crate::cli::{Cli, SnapshotArgs};

/// Runs `polis snapshot`.
pub fn snapshot(cli: &Cli, args: &SnapshotArgs) -> anyhow::Result<()> {
    let (tree, title) = if let Some(files) = args.synthetic {
        (
            synthetic::repository(files, args.seed),
            format!("POLIS / SYNTHETIC {files}-FILE REPOSITORY"),
        )
    } else {
        let root = cli.repo_root().context("resolving the repository root")?;
        let tree = index(&root, args)?;
        let name = root.file_name().map_or_else(
            || root.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        (tree, format!("POLIS / {} REPOSITORY", name.to_uppercase()))
    };

    let inputs = layout_inputs(&tree, args.synthetic.is_none());
    let started = Instant::now();
    let city = city::generate_with(&tree, &inputs);
    let generate_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let structure = city::measure(&city);

    let plan_canvas = plan::render_plan(
        &city,
        &structure,
        &title,
        args.pixels,
        args.supersample,
        args.streets,
    );
    plan_canvas
        .write_png(&args.out)
        .with_context(|| format!("writing {}", args.out.display()))?;
    println!("wrote {}", args.out.display());

    if let Some(path) = &args.junctions {
        let junctions = plan::render_junctions(
            &city,
            &structure,
            &format!("{title} - ROAD GRAPH ONLY, JUNCTIONS BY DEGREE"),
            args.pixels,
            args.supersample,
        );
        junctions
            .write_png(path)
            .with_context(|| format!("writing {}", path.display()))?;
        println!("wrote {}", path.display());
    }

    if let Some(path) = &args.layout {
        let json = city.snapshot().context("serializing the layout")?;
        std::fs::write(path, &json).with_context(|| format!("writing {}", path.display()))?;
        println!("wrote {} ({} bytes)", path.display(), json.len());
    }

    print!("{}", report(&city, &structure, generate_ms));
    println!("DIGEST      {:016x}", city.digest());
    Ok(())
}

/// Index a checkout: the file tree plus git's growth order (PRD §7.1).
///
/// # This command writes into the repository it is drawing
///
/// `polis snapshot --out docs/city.png` puts a PNG inside the checkout, and the
/// next run would walk it, give it a building, and produce a *different* city —
/// the feedback loop `polis_repo::tree::WalkExclusions` documents, which is
/// exactly how the design bake-off's town grew a few files every run.
///
/// So every path this command is about to write is excluded before the walk,
/// by name rather than by directory: `--out docs/city.png` must not delete the
/// `docs/` district from the map, only the one file.
fn index(root: &Path, args: &SnapshotArgs) -> anyhow::Result<RepoTree> {
    let mut exclusions = WalkExclusions::shipped();
    for output in [
        Some(&args.out),
        args.junctions.as_ref(),
        args.layout.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        exclusions.exclude_output(output, root);
    }
    let options = WalkOptions {
        // A `target/` tree is three orders of magnitude larger than the source
        // and PRD §8 draws it as one mass anyway.
        skip_massed: true,
        exclusions: Some(exclusions),
        ..WalkOptions::default()
    };
    let files = walk_with(root, &options).context("walking the checkout")?;
    let mut tree = RepoTree {
        root: root.to_path_buf(),
        files: BTreeMap::new(),
        worktrees: BTreeMap::new(),
        head: String::new(),
    };
    tree.worktrees
        .insert(WorktreeId::PRIMARY, root.to_path_buf());
    for meta in files {
        tree.files.insert(meta.path.clone(), meta);
    }
    History::read(root)
        .context("reading git history")?
        .apply(&mut tree);
    Ok(tree)
}

/// Streets, monuments and heights, where the repository can supply them.
fn layout_inputs(tree: &RepoTree, real: bool) -> LayoutInputs {
    let imports = polis_repo::imports::ImportGraph::build(tree);
    let diff_lines = if real {
        polis_repo::git::diff_line_counts(&tree.root)
            .unwrap_or_default()
            .into_iter()
            .collect()
    } else {
        BTreeMap::new()
    };
    LayoutInputs {
        streets: imports.cross_district_edges(),
        inbound: imports.inbound_counts(),
        diff_lines,
    }
}

/// The structural metric set, in the shape the design bake-off reported it.
// One `writeln!` per line of a fixed report. Splitting it would put the order of
// the report in one function and its content in another, and the cast is a file
// count becoming a percentage.
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
fn report(city: &City, s: &Structure, generate_ms: f64) -> String {
    use std::fmt::Write as _;
    let r = &city.report;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "ROADS       V={} E={} COMPONENTS={} CYCLES={} CROSSINGS={} DEG4+={} DANGLING={}",
        r.road_nodes,
        r.road_segments,
        r.components,
        r.cycles,
        r.crossings,
        r.complex_junctions,
        r.dangling
    );
    let _ = writeln!(
        out,
        "            junctions(deg!=2)={} 4-and-5+ share={:.1}% longest stroke={:.1}% of diameter through-streets(>=25%)={}",
        r.junctions,
        100.0 * f64::from(u32::try_from(r.complex_junctions).unwrap_or(0))
            / f64::from(u32::try_from(r.junctions.max(1)).unwrap_or(1)),
        s.longest_stroke * 100.0,
        s.through_streets
    );
    let _ = writeln!(
        out,
        "            longest stroke including the city limit = {:.1}% (the outline is not a street)",
        city::longest_stroke_with_limit(city) * 100.0
    );
    let _ = writeln!(
        out,
        "BLOCKS      count={} open={} slivers={} area p05/med/p95={:.3}/{:.3}/{:.3} p95:p05={:.1}x compactness={:.3}",
        r.blocks, r.open_blocks, r.slivers, s.block_p05, s.block_median, s.block_p95,
        s.block_hierarchy, s.compactness
    );
    let _ = writeln!(
        out,
        "            age gradient (rim block area / core) = {:.2}x",
        s.age_gradient
    );
    let _ = writeln!(
        out,
        "BUILDINGS   files={} massed={} lots={} vacant={} buildings={} unbuilt={} overflow={}",
        r.files, r.massed, r.lots, r.empty_lots, r.buildings, r.unbuilt, r.overflow
    );
    let _ = writeln!(
        out,
        "            coverage={:.1}% (core {:.1}%, rim {:.1}%) median lot fill={:.1}% on-road={} outside-lot={}",
        s.coverage * 100.0,
        s.coverage_core * 100.0,
        s.coverage_rim * 100.0,
        s.lot_fill * 100.0,
        s.buildings_on_road,
        s.buildings_outside_lot
    );
    let _ = writeln!(
        out,
        "DISTRICTS   count={} fragmented={} fragmented packages={} plots={} monuments={} industrial={}",
        r.districts,
        r.fragmented_districts,
        r.fragmented_packages,
        r.plots,
        r.monuments,
        city.industrial.len()
    );
    let _ = writeln!(
        out,
        "            in own territory={} on its fringe={} to ancestor={} anywhere={} detached={} rule-A bends={} shared faces={} faceless={} unhoused={}",
        r.plots
            - r.settled_on_fringe
            - r.relaxed_to_ancestor
            - r.relaxed_to_anywhere
            - r.detached_placements,
        r.settled_on_fringe,
        r.relaxed_to_ancestor,
        r.relaxed_to_anywhere,
        r.detached_placements,
        r.settled_nonadjacent,
        r.shared_faces,
        r.faceless_districts,
        r.unhoused
    );
    let _ = writeln!(
        out,
        "            plots off their face={} empty cells={}",
        r.plots_off_face, r.empty_cells
    );
    let _ = writeln!(
        out,
        "HISTORY     age ramp={} history={} days first-year files={} ({:.1}%)",
        r.age_ramp.label(),
        r.history_days,
        r.old_town_files,
        100.0 * r.old_town_files as f64 / r.files.max(1) as f64
    );
    let d = polis_layout::city::street_diagnostics(city);
    let _ = writeln!(
        out,
        "STREETS     routed={} hubs={} isolated={} widest={} import edges (PRD 9)",
        city.layout.streets.len(),
        d.hubs,
        d.isolated,
        d.widest
    );
    let _ = writeln!(
        out,
        "TIMING      full generation = {generate_ms:.1} ms  (PRD 13.1 budget: 3000 ms)"
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_synthetic_repository_renders_and_reports() {
        let tree = synthetic::repository(200, 0x99);
        let inputs = layout_inputs(&tree, false);
        let city = city::generate_with(&tree, &inputs);
        let structure = city::measure(&city);
        let text = report(&city, &structure, 1.0);
        assert!(text.contains("ROADS"), "{text}");
        assert!(
            text.contains("COVERAGE") || text.contains("coverage"),
            "{text}"
        );
        let canvas = plan::render_plan(&city, &structure, "T", 200, 1, false);
        assert_eq!(canvas.width, 200);
    }
}
