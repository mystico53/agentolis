//! Prints a repository's neighborhoods: the report behind
//! `docs/neighborhoods-sample.md`.
//!
//! ```text
//! cargo run --release -p polis-repo --example neighborhoods -- <repo> [--top N] [--markdown] [--check]
//! ```
//!
//! `--check` builds the partition twice, in one process, and compares the
//! serialized result byte for byte — PRD §7.4's determinism requirement, run on
//! a real repository rather than a fixture.
//!
//! Read-only. It opens files for their first 16 KiB and writes nothing into the
//! repository it is pointed at.

use std::collections::BTreeMap;
use std::path::PathBuf;

use polis_repo::describe::DescriptionSource;
use polis_repo::imports::ImportGraph;
use polis_repo::kinds::CodeKind;
use polis_repo::neighborhoods::{NeighborhoodConfig, NeighborhoodOptions, Neighborhoods};
use polis_repo::tree::{DistrictTree, RepoIndex};
use polis_repo::RepoTree;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut root: Option<PathBuf> = None;
    let mut top = 15usize;
    let mut markdown = false;
    let mut check = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--markdown" => markdown = true,
            "--check" => check = true,
            "--top" => top = args.next().and_then(|n| n.parse().ok()).unwrap_or(15),
            other => root = Some(PathBuf::from(other)),
        }
    }
    let Some(root) = root else {
        eprintln!("usage: neighborhoods <repo> [--top N] [--markdown] [--check]");
        std::process::exit(2);
    };

    let index = RepoIndex::open(&root)?;
    let tree = index.tree();
    let config = NeighborhoodConfig::for_repo(&root);
    let options = NeighborhoodOptions::from_config(&config);

    let mut hoods = Neighborhoods::build(tree, &options);
    if check {
        let again = Neighborhoods::build(tree, &options);
        let a = serde_json::to_string(&hoods)?;
        let b = serde_json::to_string(&again)?;
        println!(
            "determinism: {} ({} bytes)",
            if a == b { "identical" } else { "DIFFERENT" },
            a.len()
        );
        if a != b {
            std::process::exit(1);
        }
    }

    // The import graph, over the operator's own files only: parsing 80 000
    // vendored JavaScript files to find a monument inside somebody else's
    // package is a cost with no reader.
    let civic = civic_only(tree, &options);
    let graph = ImportGraph::build(&civic);
    hoods.set_monuments(&graph.inbound_counts());
    hoods.describe(&root, tree, None);

    let districts = DistrictTree::from_repo(tree, options.rules.industrial());
    let top_level = tree
        .files
        .keys()
        .filter_map(|p| p.components().next())
        .collect::<std::collections::BTreeSet<_>>()
        .len();

    if markdown {
        print_markdown(&root, &hoods, top_level, districts.len(), top);
    } else {
        print_plain(&root, &hoods, top_level, districts.len(), top);
    }
    Ok(())
}

/// A copy of the tree with every industrial file removed.
fn civic_only(tree: &RepoTree, options: &NeighborhoodOptions) -> RepoTree {
    let rules = options.rules.industrial();
    let mut out = tree.clone();
    out.files.retain(|path, _| !rules.is_industrial_file(path));
    out
}

/// The kind breakdown of a whole repository, as `name=count` pairs.
fn kind_line(hoods: &Neighborhoods) -> String {
    let mix = hoods.mix();
    CodeKind::ALL
        .into_iter()
        .filter(|k| mix.count(*k) > 0)
        .map(|k| format!("{}={}", k.name(), mix.count(k)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// How many neighborhoods came from each description source.
fn sources(hoods: &Neighborhoods) -> BTreeMap<&'static str, u32> {
    let mut out: BTreeMap<&'static str, u32> = BTreeMap::new();
    for hood in hoods.all() {
        let name = hood
            .description
            .as_ref()
            .map_or("none", |d| d.source.name());
        *out.entry(name).or_insert(0) += 1;
    }
    out
}

fn print_plain(
    root: &std::path::Path,
    hoods: &Neighborhoods,
    top_level: usize,
    directories: usize,
    top: usize,
) {
    let stats = hoods.stats();
    println!("== {} ==", root.display());
    println!(
        "files {} (civic {}, vendored {})",
        stats.total_files, stats.civic_files, stats.vendored_files
    );
    println!(
        "districts: top-level {top_level} -> neighborhoods {} (every directory would be {directories})",
        stats.districts
    );
    println!(
        "bounds: floor {} files, ceiling {} files",
        stats.min_files, stats.max_files
    );
    println!("kinds: {}", kind_line(hoods));
    println!(
        "described {}/{} ({:.0}% prose), sources {:?}",
        stats.described,
        stats.districts,
        stats.prose_coverage() * 100.0,
        sources(hoods)
    );
    println!(
        "describe: {} files read, {} cache hits, rejected {} (short {}, code {}, secret {})",
        stats.describe.files_read,
        stats.describe.cache_hits,
        stats.describe.rejected(),
        stats.describe.rejected_short,
        stats.describe.rejected_code,
        stats.describe.rejected_secret
    );
    println!();
    for hood in hoods.by_size().into_iter().take(top) {
        println!(
            "{:<28} {:<9} {:>6}  {}",
            hood.name,
            hood.kind.name(),
            hood.file_count,
            hood.label().unwrap_or("")
        );
    }
}

fn print_markdown(
    root: &std::path::Path,
    hoods: &Neighborhoods,
    top_level: usize,
    directories: usize,
    top: usize,
) {
    let stats = hoods.stats();
    let name = root.file_name().map_or_else(
        || root.display().to_string(),
        |n| n.to_string_lossy().into(),
    );
    println!("### `{name}`");
    println!();
    println!(
        "{} files ({} civic, {} vendored) · top-level directories **{top_level}** → neighborhoods **{}** \
         · every directory would be {directories} · floor {} / ceiling {} files",
        stats.total_files,
        stats.civic_files,
        stats.vendored_files,
        stats.districts,
        stats.min_files,
        stats.max_files
    );
    println!();
    println!("Kinds: {}", kind_line(hoods));
    println!();
    println!(
        "Described **{}/{}**, of which **{}** is prose a human wrote ({:.0}%). Sources: {:?}. \
         Rejected {} candidates (short {}, code {}, credential-shaped {}).",
        stats.described,
        stats.districts,
        stats.described_prose,
        stats.prose_coverage() * 100.0,
        sources(hoods),
        stats.describe.rejected(),
        stats.describe.rejected_short,
        stats.describe.rejected_code,
        stats.describe.rejected_secret
    );
    println!();
    println!("| neighborhood | kind | files | description | source |");
    println!("|---|---|---:|---|---|");
    for hood in hoods.by_size().into_iter().take(top) {
        let (label, source) = match &hood.description {
            Some(d) => (escape(&d.label), d.source.name()),
            None => (String::new(), "—"),
        };
        let source = if source == DescriptionSource::Inventory.name() {
            "*inventory*"
        } else {
            source
        };
        println!(
            "| `{}` | {} | {} | {} | {} |",
            escape(&hood.name),
            hood.kind.name(),
            hood.file_count,
            label,
            source
        );
    }
    println!();
}

/// Escapes the two characters that would break a Markdown table cell.
fn escape(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}
