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

/// Where the wall clock of a cold start goes (PRD §13.1: under 3 s at 5 000
/// files).
///
/// Every stage between "the process started" and "there is a frame", timed
/// separately, because the budget was blown by the one stage nobody was
/// measuring: two full `git log` walks of the whole history, at over a second
/// each on a repository of Django's age.
// Every field is a duration and the unit is the point of the name: `walk` alone
// would read as a count, and the report prints them side by side in one row.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy, Default)]
struct Phases {
    /// Walking the checkout: `read_dir`, classify, `stat`.
    walk_ms: f64,
    /// `polis_repo::git::History` — the growth order and last-touched times.
    history_ms: f64,
    /// tree-sitter over every source file (PRD §9).
    imports_ms: f64,
    /// `git diff --numstat` for PRD §7.3's building heights.
    diff_ms: f64,
    /// The layout itself (PRD §7.2).
    generate_ms: f64,
    /// Rasterising the plan and writing the PNG.
    render_ms: f64,
}

impl Phases {
    /// Everything from process start to a frame on screen.
    fn total_ms(self) -> f64 {
        self.walk_ms
            + self.history_ms
            + self.imports_ms
            + self.diff_ms
            + self.generate_ms
            + self.render_ms
    }
}

/// Runs `polis snapshot`.
pub fn snapshot(cli: &Cli, args: &SnapshotArgs) -> anyhow::Result<()> {
    let mut phases = Phases::default();
    let (tree, title) = if let Some(files) = args.synthetic {
        (
            synthetic::repository(files, args.seed),
            format!("POLIS / SYNTHETIC {files}-FILE REPOSITORY"),
        )
    } else {
        let root = cli.repo_root().context("resolving the repository root")?;
        let tree = index(&root, args, &mut phases)?;
        let name = root.file_name().map_or_else(
            || root.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        (tree, format!("POLIS / {} REPOSITORY", name.to_uppercase()))
    };

    let inputs = layout_inputs(&tree, args.synthetic.is_none(), &mut phases);
    let started = Instant::now();
    let city = city::generate_with(&tree, &inputs);
    phases.generate_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let structure = city::measure(&city);

    let started = Instant::now();
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
    phases.render_ms = started.elapsed().as_secs_f64() * 1_000.0;
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

    print!("{}", report(&city, &structure, phases));
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
///
/// # The history is read through its cache
///
/// PRD §7.1 asks for the derived growth sequence to be "cached keyed on `HEAD`"
/// and recomputed incrementally, and PRD §13.1 budgets the whole cold start at
/// under 3 s. Reading it uncached costs a full `git log` walk of the entire
/// history — 1.9 s of Django's 3.1 s — so the product path uses
/// [`History::read_cached_default`], which is the cache PRD §7.1 specifies.
fn index(root: &Path, args: &SnapshotArgs, phases: &mut Phases) -> anyhow::Result<RepoTree> {
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
    let started = Instant::now();
    let files = walk_with(root, &options).context("walking the checkout")?;
    phases.walk_ms = started.elapsed().as_secs_f64() * 1_000.0;
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
    let started = Instant::now();
    History::read_cached_default(root)
        .context("reading git history")?
        .apply(&mut tree);
    phases.history_ms = started.elapsed().as_secs_f64() * 1_000.0;
    Ok(tree)
}

/// Streets, monuments and heights, where the repository can supply them.
fn layout_inputs(tree: &RepoTree, real: bool, phases: &mut Phases) -> LayoutInputs {
    let started = Instant::now();
    let imports = polis_repo::imports::ImportGraph::build(tree);
    phases.imports_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let started = Instant::now();
    let diff_lines = if real {
        polis_repo::git::diff_line_counts(&tree.root)
            .unwrap_or_default()
            .into_iter()
            .collect()
    } else {
        BTreeMap::new()
    };
    phases.diff_ms = started.elapsed().as_secs_f64() * 1_000.0;
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
fn report(city: &City, s: &Structure, phases: Phases) -> String {
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
    // Is it a place, or a diagram? The two numbers three M1 gates were failed
    // on, printed where the rest of the structure is (PRD §15).
    let _ = writeln!(
        out,
        "            solidity={:.4} (area / convex hull; a coin is 1.00) radial spokes={} boulevards-through-the-middle={}",
        s.solidity, s.radial_spokes, s.radial_strokes
    );
    let _ = writeln!(
        out,
        "            longest dead-straight district border={:.1}% of diameter, past a fifth of it={} (a ruler, whatever its bearing)",
        s.straight_border * 100.0,
        s.straight_borders
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
        "HISTORY     age ramp={} history={} days first-year files={} ({:.1}%) core files={} ({:.1}%) equalisation={}%",
        r.age_ramp.label(),
        r.history_days,
        r.old_town_files,
        100.0 * r.old_town_files as f64 / r.files.max(1) as f64,
        r.core_files,
        100.0 * r.core_files as f64 / r.files.max(1) as f64,
        r.age_equalisation_x100
    );
    // PRD §7.3 makes height the primary encoded quantity — "the tallest thing on
    // the map is the biggest unreviewed pile" — so the report says which
    // building that is. It is the one line an operator can check the picture
    // against, and it is what caught the earlier renders drawing every building
    // at one tone: on a clean checkout the whole column is the base height, and
    // that is a fact about the repository rather than about the renderer.
    let mut heights: Vec<f32> = city.layout.buildings.values().map(|b| b.height).collect();
    heights.sort_by(f32::total_cmp);
    let tallest = city.layout.buildings.values().max_by(|a, b| {
        a.height
            .total_cmp(&b.height)
            .then_with(|| b.path.cmp(&a.path))
    });
    let _ = writeln!(
        out,
        "HEIGHT      distinct={} min/median/max={:.2}/{:.2}/{:.2} tallest={} (PRD 7.3: uncommitted diff lines)",
        {
            let mut seen: Vec<f32> = heights.clone();
            seen.dedup_by(|a, b| a.total_cmp(b).is_eq());
            seen.len()
        },
        heights.first().copied().unwrap_or(0.0),
        heights.get(heights.len() / 2).copied().unwrap_or(0.0),
        heights.last().copied().unwrap_or(0.0),
        tallest.map_or_else(|| "none".to_owned(), |b| b.path.as_str().to_owned())
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
        "TIMING      cold start to first frame = {:.0} ms  (PRD 13.1 budget: 3000 ms)",
        phases.total_ms()
    );
    let _ = writeln!(
        out,
        "            walk={:.0} history={:.0} imports={:.0} diff={:.0} layout={:.0} render={:.0} ms",
        phases.walk_ms,
        phases.history_ms,
        phases.imports_ms,
        phases.diff_ms,
        phases.generate_ms,
        phases.render_ms
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_synthetic_repository_renders_and_reports() {
        let tree = synthetic::repository(200, 0x99);
        let mut phases = Phases::default();
        let inputs = layout_inputs(&tree, false, &mut phases);
        let city = city::generate_with(&tree, &inputs);
        let structure = city::measure(&city);
        let text = report(&city, &structure, phases);
        assert!(text.contains("ROADS"), "{text}");
        assert!(
            text.contains("COVERAGE") || text.contains("coverage"),
            "{text}"
        );
        let canvas = plan::render_plan(&city, &structure, "T", 200, 1, false);
        assert_eq!(canvas.width, 200);
    }
}
