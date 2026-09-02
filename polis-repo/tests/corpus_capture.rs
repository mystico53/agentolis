//! Captures a real repository into a checked-in corpus manifest.
//!
//! The fixtures in `tests/corpora/` are the M1 gate's **real** input (PRD §16).
//! They are captured here rather than by a shell script so the capture uses the
//! same walk, the same classifier and the same growth-order reader the product
//! uses — a fixture built by a second implementation of those rules is a fixture
//! that can disagree with the product and look like a layout bug.
//!
//! # Regenerating
//!
//! ```text
//! git clone https://github.com/pallets/click.git /tmp/click
//! git -C /tmp/click checkout <the pinned commit in tests/corpora/README.md>
//! POLIS_CAPTURE_ROOT=/tmp/click POLIS_CAPTURE_NAME=click \
//!   POLIS_CAPTURE_ORIGIN=https://github.com/pallets/click.git \
//!   cargo test -p polis-repo --test corpus_capture -- --ignored --nocapture
//! ```
//!
//! It writes `tests/corpora/<name>.corpus` at the workspace root and prints the
//! header. **Pin the new commit in `tests/corpora/README.md` in the same
//! change**: a fixture whose provenance is not written down is a fixture nobody
//! can check.
//!
//! There is deliberately no network access anywhere in the test suite. The
//! capture is a developer action; CI reads the checked-in file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use polis_events::WorktreeId;
use polis_repo::git::History;
use polis_repo::manifest::{self, CorpusHeader};
use polis_repo::tree::{walk_with, WalkOptions};
use polis_repo::RepoTree;

/// The workspace root — the directory holding `Cargo.toml` and `tests/`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("polis-repo has a parent directory")
        .to_path_buf()
}

/// Reads one checkout the way the product does.
fn read_checkout(root: &Path, skip_massed: bool) -> RepoTree {
    let options = WalkOptions {
        skip_massed,
        ..WalkOptions::default()
    };
    let files = walk_with(root, &options).expect("walk the checkout");
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
        .expect("read git history")
        .apply(&mut tree);
    tree
}

/// `git rev-list --count HEAD`, or zero if git will not say.
fn commit_count(root: &Path) -> u64 {
    std::process::Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// See the module documentation. Ignored: it needs a checkout on disk and it
/// writes into the repository.
#[test]
#[ignore = "developer action: needs a checkout named by POLIS_CAPTURE_ROOT"]
fn capture_a_corpus() {
    let root = PathBuf::from(
        std::env::var_os("POLIS_CAPTURE_ROOT")
            .expect("set POLIS_CAPTURE_ROOT to a checkout of the repository to capture"),
    );
    let name = std::env::var("POLIS_CAPTURE_NAME").unwrap_or_else(|_| {
        root.file_name()
            .map_or_else(|| "corpus".to_owned(), |n| n.to_string_lossy().into_owned())
    });
    let origin = std::env::var("POLIS_CAPTURE_ORIGIN").unwrap_or_default();
    let skip_massed = std::env::var("POLIS_CAPTURE_SKIP_MASSED").is_ok_and(|v| v == "1");

    let tree = read_checkout(&root, skip_massed);
    let header = CorpusHeader {
        name: name.clone(),
        origin,
        head: polis_repo::git::head_commit(&root).expect("HEAD"),
        commits: commit_count(&root),
        skip_massed,
    };
    let text = manifest::capture(&tree, &header);
    let out = workspace_root()
        .join("tests")
        .join("corpora")
        .join(format!("{name}.corpus"));
    std::fs::create_dir_all(out.parent().expect("has a parent")).expect("create tests/corpora");
    std::fs::write(&out, text.as_bytes())
        .unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
    println!(
        "POLIS_CAPTURE wrote {} — {} files, head {}, {} commits",
        out.display(),
        tree.files.len(),
        header.head,
        header.commits
    );
}
