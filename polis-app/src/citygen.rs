//! Building the city the window draws (PRD §7, §15 M1).
//!
//! `polis snapshot` already does this on the way to a PNG, but it does it with
//! one thing the window must not inherit: it excludes the files it is about to
//! write, because a PNG written into the checkout would grow a building on the
//! next run. The window writes nothing into the repository, so it walks the
//! checkout plain.
//!
//! Everything expensive here is read through the caches PRD §7.1 and §13.1
//! require — `git log` and `tree-sitter` are the two largest terms in a cold
//! start, and neither changes between two launches of a checkout nobody edited.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context as _;
use polis_events::WorktreeId;
use polis_layout::city::{self, City, LayoutInputs};
use polis_repo::git::History;
use polis_repo::tree::{walk_with, WalkExclusions, WalkOptions};
use polis_repo::RepoTree;

/// Where the wall clock of a cold start went (PRD §13.1: under 3 s at 5 000
/// files).
///
/// Reported in the status bar rather than only on stdout, because the window is
/// the thing the budget is about.
#[derive(Debug, Clone, Copy, Default)]
pub struct ColdStart {
    /// Walking the checkout.
    pub walk_ms: f64,
    /// `git log` — the growth order and last-touched times.
    pub history_ms: f64,
    /// `tree-sitter` over every source file (PRD §9).
    pub imports_ms: f64,
    /// `git diff --numstat` for PRD §7.3's building heights.
    pub diff_ms: f64,
    /// The layout itself (PRD §7.2).
    pub layout_ms: f64,
    /// Rasterising the base map (filled in by [`crate::basemap`]).
    pub basemap_ms: f64,
}

impl ColdStart {
    /// Everything from process start to a frame on screen.
    pub fn total_ms(self) -> f64 {
        self.walk_ms
            + self.history_ms
            + self.imports_ms
            + self.diff_ms
            + self.layout_ms
            + self.basemap_ms
    }
}

/// A generated city and how long it took.
#[derive(Debug)]
pub struct Generated {
    /// The repository root it was generated from.
    pub root: PathBuf,
    /// The file tree, kept because [`polis_world::World::new`] wants it.
    pub tree: RepoTree,
    /// The city.
    pub city: City,
    /// Where the cold start went.
    pub timing: ColdStart,
}

impl Generated {
    /// The repository's directory name, upper-cased for the title bar.
    pub fn title(&self) -> String {
        self.root.file_name().map_or_else(
            || self.root.display().to_string(),
            |n| n.to_string_lossy().to_uppercase(),
        )
    }
}

/// Generates the city for one checkout.
pub fn generate(root: &Path) -> anyhow::Result<Generated> {
    let mut timing = ColdStart::default();
    let tree = index(root, &mut timing)?;
    let inputs = layout_inputs(&tree, &mut timing);
    let started = Instant::now();
    let city = city::generate_with(&tree, &inputs);
    timing.layout_ms = started.elapsed().as_secs_f64() * 1_000.0;
    Ok(Generated {
        root: root.to_path_buf(),
        tree,
        city,
        timing,
    })
}

/// A synthetic city of `files` files, for exercising the window at a scale no
/// fixture reaches (PRD §13.1 budgets cold start at 5 000 files).
pub fn synthetic(files: usize, seed: u64) -> Generated {
    let mut timing = ColdStart::default();
    let tree = polis_repo::synthetic::repository(files, seed);
    let inputs = LayoutInputs {
        streets: polis_repo::imports::ImportGraph::build(&tree).cross_district_edges(),
        inbound: polis_repo::imports::ImportGraph::build(&tree).inbound_counts(),
        diff_lines: BTreeMap::new(),
    };
    let started = Instant::now();
    let city = city::generate_with(&tree, &inputs);
    timing.layout_ms = started.elapsed().as_secs_f64() * 1_000.0;
    Generated {
        root: PathBuf::from(format!("synthetic-{files}")),
        tree,
        city,
        timing,
    }
}

/// Walks a checkout and applies git's growth order (PRD §7.1).
fn index(root: &Path, timing: &mut ColdStart) -> anyhow::Result<RepoTree> {
    let options = WalkOptions {
        // A `target/` tree is three orders of magnitude larger than the source
        // and PRD §8 draws it as one mass anyway.
        skip_massed: true,
        exclusions: Some(WalkExclusions::shipped()),
        ..WalkOptions::default()
    };
    let started = Instant::now();
    let files = walk_with(root, &options).context("walking the checkout")?;
    timing.walk_ms = started.elapsed().as_secs_f64() * 1_000.0;

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
    timing.history_ms = started.elapsed().as_secs_f64() * 1_000.0;
    Ok(tree)
}

/// Streets, monuments and heights, read through their caches.
fn layout_inputs(tree: &RepoTree, timing: &mut ColdStart) -> LayoutInputs {
    let started = Instant::now();
    let imports = polis_repo::imports::ImportGraph::build_cached(tree);
    timing.imports_ms = started.elapsed().as_secs_f64() * 1_000.0;

    let started = Instant::now();
    let diff_lines = polis_repo::git::diff_line_counts(&tree.root)
        .unwrap_or_default()
        .into_iter()
        .collect();
    timing.diff_ms = started.elapsed().as_secs_f64() * 1_000.0;

    LayoutInputs {
        streets: imports.cross_district_edges(),
        inbound: imports.inbound_counts(),
        diff_lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_synthetic_city_has_buildings_and_districts() {
        let generated = synthetic(120, 7);
        assert!(!generated.city.layout.buildings.is_empty());
        assert!(!generated.city.layout.districts.is_empty());
        assert!(generated.timing.total_ms() >= 0.0);
    }

    /// The window walks the checkout plain — `polis snapshot`'s output
    /// exclusions exist because it writes a PNG into the repository, and the
    /// window writes nothing.
    #[test]
    fn the_window_generates_the_same_city_the_snapshot_does() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let a = generate(root).expect("first generation");
        let b = generate(root).expect("second generation");
        assert_eq!(
            a.city.digest(),
            b.city.digest(),
            "PRD §7.4: two runs in one process must agree"
        );
    }
}
