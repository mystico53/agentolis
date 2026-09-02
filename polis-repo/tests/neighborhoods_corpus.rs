//! The neighborhood partition, run on the **real repositories** ADR-0080
//! checked in as corpus manifests.
//!
//! A generator can flatter a partition rule the same way ADR-0078 showed it
//! flattering a layout: a synthetic tree has an even branching factor, no
//! vendored trees, and no `locale/` directory with four hundred `.po` files in
//! it. Every claim in `polis_repo::neighborhoods`' module documentation is
//! therefore asserted here against `click`, `pytest` and `polis-day-one`.
//!
//! A manifest carries no file content (ADR-0080), so this covers the partition,
//! the kinds and the naming — everything except the descriptions, which need a
//! checkout and are covered by the unit tests in `polis_repo::describe`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use polis_events::LogicalPath;
use polis_repo::kinds::CodeKind;
use polis_repo::manifest;
use polis_repo::neighborhoods::{NeighborhoodOptions, Neighborhoods};

/// The workspace root — the directory holding `Cargo.toml` and `tests/`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("polis-repo has a parent directory")
        .to_path_buf()
}

/// Every checked-in corpus, loaded.
fn corpora() -> Vec<(String, Neighborhoods, usize)> {
    let dir = workspace_root().join("tests").join("corpora");
    let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("tests/corpora exists")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "corpus"))
        .collect();
    // `read_dir` is filesystem order, which differs between NTFS, ext4 and
    // APFS. Sort it, like every other directory listing in this crate.
    names.sort();
    assert!(!names.is_empty(), "no corpora in {}", dir.display());
    names
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).expect("corpus is readable");
            let corpus = manifest::load(&text).expect("corpus parses");
            let files = corpus.tree.files.len();
            let hoods = corpus.tree.neighborhoods(&NeighborhoodOptions::default());
            (corpus.header.name.clone(), hoods, files)
        })
        .collect()
}

#[test]
fn every_file_lands_in_exactly_one_neighborhood() {
    let dir = workspace_root().join("tests").join("corpora");
    for entry in std::fs::read_dir(&dir).expect("tests/corpora exists") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|e| e != "corpus") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("readable");
        let corpus = manifest::load(&text).expect("parses");
        let hoods = corpus.tree.neighborhoods(&NeighborhoodOptions::default());
        let name = &corpus.header.name;

        let mut seen = 0usize;
        for file in corpus.tree.files.keys() {
            let owner = hoods
                .district_of(file)
                .unwrap_or_else(|| panic!("{name}: {file} has no neighborhood"));
            assert!(
                file.starts_with(&owner.path),
                "{name}: {file} was given to {}",
                owner.path
            );
            seen += 1;
        }
        assert_eq!(seen, corpus.tree.files.len());
        let summed: u32 = hoods.all().iter().map(|n| n.file_count).sum();
        assert_eq!(
            summed as usize,
            corpus.tree.files.len(),
            "{name}: the file counts must partition the repository"
        );
    }
}

#[test]
fn the_partition_is_finer_than_the_top_level_and_coarser_than_every_directory() {
    for (name, hoods, files) in corpora() {
        let stats = hoods.stats();
        let directories: BTreeSet<LogicalPath> =
            hoods.all().iter().map(|n| n.path.clone()).collect();
        assert_eq!(directories.len(), hoods.len(), "{name}: paths are unique");
        assert!(
            stats.districts >= 2,
            "{name}: {files} files collapsed to {} neighborhood(s)",
            stats.districts
        );
        // Every neighborhood is a directory, and the deepest is not absurd:
        // PRD §9's "the tree determines placement" is about granularity here,
        // not about inventing a structure the tree does not have.
        for hood in hoods.all() {
            assert!(
                hood.depth <= NeighborhoodOptions::default().max_depth,
                "{name}: {} is {} deep",
                hood.path,
                hood.depth
            );
        }
    }
}

#[test]
fn names_are_unique_within_a_repository() {
    for (name, hoods, _) in corpora() {
        let mut names: Vec<&str> = hoods.all().iter().map(|n| n.name.as_str()).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            total,
            "{name}: two neighborhoods share a label, which makes both unfindable"
        );
    }
}

#[test]
fn a_python_repository_is_mostly_source_and_test_and_says_which() {
    // Eyeballing the classifier, as an assertion. `click` and `pytest` are
    // Python libraries with a real test suite and real documentation; a rule set
    // that cannot find any of the three is wrong however clean its unit tests
    // are.
    for (name, hoods, _) in corpora() {
        if name != "click" && name != "pytest" {
            continue;
        }
        let mix = hoods.mix();
        assert!(
            mix.count(CodeKind::Test) > 0,
            "{name}: no tests found in a repository that has a test suite"
        );
        assert!(mix.count(CodeKind::Source) > 0, "{name}: no source found");
        assert!(
            mix.count(CodeKind::Docs) > 0,
            "{name}: no documentation found"
        );
        assert!(
            mix.share(CodeKind::Unknown) < 0.25,
            "{name}: {:.0}% of the files match no rule at all",
            mix.share(CodeKind::Unknown) * 100.0
        );
        // And the test estate is visible as a district, not just as a file
        // count: this is the reading the operator asked for.
        assert!(
            hoods.all().iter().any(|n| n.kind == CodeKind::Test),
            "{name}: no neighborhood reads as tests"
        );
    }
}

#[test]
fn two_builds_of_the_same_corpus_are_byte_identical() {
    // PRD §7.4, on real input, in one process — the cheap half of the
    // determinism claim. The permutation half is asserted in the unit tests.
    for (name, hoods, _) in corpora() {
        let first = serde_json::to_string(&hoods).expect("json");
        let dir = workspace_root().join("tests").join("corpora");
        let path = dir.join(format!("{name}.corpus"));
        let text = std::fs::read_to_string(&path).expect("readable");
        let corpus = manifest::load(&text).expect("parses");
        let again = corpus.tree.neighborhoods(&NeighborhoodOptions::default());
        let second = serde_json::to_string(&again).expect("json");
        assert_eq!(first, second, "{name}: two builds disagree");
    }
}

#[test]
fn the_partition_reports_what_it_did() {
    // The numbers the report is built from have to be internally consistent, or
    // the honesty they exist for is theatre.
    for (name, hoods, files) in corpora() {
        let stats = hoods.stats();
        assert_eq!(stats.total_files as usize, files, "{name}");
        assert_eq!(
            stats.civic_files + stats.vendored_files,
            stats.total_files,
            "{name}: civic and vendored must cover every file"
        );
        assert_eq!(stats.districts as usize, hoods.len(), "{name}");
        assert_eq!(stats.mix.total(), stats.total_files, "{name}");
        assert!(stats.max_files >= stats.min_files, "{name}");
        // Nothing has been described yet: `Neighborhoods::build` does no I/O.
        assert_eq!(stats.described, 0, "{name}");
        assert_eq!(stats.describe.files_read, 0, "{name}");
    }
}
