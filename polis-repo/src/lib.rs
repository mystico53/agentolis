//! `polis-repo` — the repository index (PRD §6.1, §7.1, §9, §14).
//!
//! Everything the city is *made of*, before anything is laid out: the file tree,
//! the growth order that gives the city its age structure, the cross-district
//! import graph that becomes its streets, and the corpus statistics that stop
//! every agent's territory from claiming the README.
//!
//! | Module | PRD section |
//! |---|---|
//! | [`tree`] | §7.1, §8 — walking the repository and classifying what is in it |
//! | [`git`] | §7.1, §7.6 — growth order from `git log`, worktrees, diff counts |
//! | [`imports`] | §9 — tree-sitter import extraction and the streets it becomes |
//! | [`corpus`] | §6.1 — the TF-IDF ubiquity discount |
//! | [`manifest`] | §16 — a real repository, recorded so CI can lay it out without a clone |
//!
//! # Determinism reaches this crate, not just `polis-layout`
//!
//! PRD §7.4's promise — "the same repo produces the same city on every launch
//! **and on every machine**" — is only as strong as its input. [`RepoTree`] is
//! what `polis-layout` consumes, so:
//!
//! * **`BTreeMap` everywhere iteration order can reach the layout.** Not
//!   `HashMap`, and not `ahash`: `ahash`'s `RandomState` is seeded per process,
//!   so an `AHashMap` iterated for output produces a different city every launch
//!   on the same machine. `ahash` is declared for hot lookup tables that are
//!   never iterated for output, and for nothing else.
//! * **Directory walks are sorted.** `read_dir` order is filesystem order, which
//!   differs between NTFS, ext4 and APFS. Sort before doing anything with it.
//! * **No wall clock.** [`WallTime`] values in this crate come from git, never
//!   from `SystemTime::now()`. The one legitimate "now" is PRD §8's 90-day
//!   overgrowth test, and that is a *rendering* input, passed in explicitly, not
//!   read here.
//!
//! # Ownership of the shared surface
//!
//! The types in this file — [`RepoTree`], [`FileMeta`], [`FileClass`],
//! [`Language`], [`ImportEdge`], [`ImportTarget`] and [`RepoDelta`] — are the
//! crate's **cross-module contract**, and [`RepoTree`] and [`RepoDelta`] are also
//! `polis-layout`'s input. Their names and shapes are fixed; the code that fills
//! them in is not.

pub mod corpus;
pub mod git;
pub mod imports;
pub mod manifest;
pub mod synthetic;
pub mod tree;

use std::collections::BTreeMap;
use std::path::PathBuf;

use polis_events::{LogicalPath, WallTime, WorktreeId};
use serde::{Deserialize, Serialize};

/// The file tree the city is generated from (PRD §5, `World::repo`).
///
/// `BTreeMap`, not `HashMap`: PRD §7.4 requires that iteration order can never
/// reach the layout, and this map's order does.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoTree {
    /// Absolute path of the primary checkout.
    pub root: PathBuf,
    /// Every tracked file, ordered by logical path.
    pub files: BTreeMap<LogicalPath, FileMeta>,
    /// Registered worktrees (PRD §7.6). One shared base map, tinted per
    /// worktree — never one city each. [`WorktreeId::PRIMARY`] maps to
    /// [`RepoTree::root`].
    pub worktrees: BTreeMap<WorktreeId, PathBuf>,
    /// `HEAD` the tree was indexed at. A mismatch invalidates every cache keyed
    /// on it, including [`git::GrowthSequence`].
    pub head: String,
}

impl RepoTree {
    /// Metadata for one file.
    pub fn file(&self, path: &LogicalPath) -> Option<&FileMeta> {
        self.files.get(path)
    }

    /// Every file directly inside `district`, in path order.
    ///
    /// A district is a directory (PRD §3), and the directory tree — not the
    /// import graph — determines placement (PRD §9).
    pub fn files_in<'a>(
        &'a self,
        district: &LogicalPath,
    ) -> impl Iterator<Item = &'a FileMeta> + 'a {
        // Cloned into the closure so the returned iterator borrows only `self`;
        // tying it to the district's lifetime as well makes every call site pass
        // a named binding for no benefit.
        let district = district.clone();
        self.files
            .values()
            .filter(move |f| f.path.parent().as_ref() == Some(&district))
    }

    /// Every file under `district`, at any depth, in path order.
    pub fn files_under<'a>(
        &'a self,
        district: &LogicalPath,
    ) -> impl Iterator<Item = &'a FileMeta> + 'a {
        let district = district.clone();
        self.files
            .values()
            .filter(move |f| f.path.starts_with(&district))
    }
}

/// What the index knows about one file before any agent touches it.
///
/// Carries its own [`path`](Self::path) even though [`RepoTree::files`] is keyed
/// on it: a `FileMeta` is passed to `polis-layout` on its own — a lot needs its
/// occupant's path to seed every draw (PRD §7.4) — and a struct that has to be
/// paired with its key to be useful gets separated from it eventually.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    /// Logical path, worktree prefix stripped (PRD §7.6). **The layout key**, and
    /// the seed for every random draw about this file (PRD §7.4).
    pub path: LogicalPath,
    /// Size in bytes. Footprint area is proportional to its square root,
    /// clamped to `[min_lot, block_area * 0.6]` (PRD §7.3).
    pub size_bytes: u64,
    /// Position in the growth sequence — the index of the commit that first
    /// added this file. Files added in the repo's first year form the old town;
    /// files added last month sit on the periphery (PRD §7.1).
    pub growth_index: u32,
    /// Commit time of that first addition. Zero-valued
    /// ([`WallTime::UNIX_EPOCH`]) for a file git has never seen — an untracked
    /// working-tree file still gets a building.
    pub added_at: WallTime,
    /// Commit time of the last commit that touched it. Drives PRD §8's
    /// overgrowth at 90 days, via [`WallTime::days_until`].
    pub last_touched: WallTime,
    /// How the landmark layer should treat it (PRD §8).
    pub class: FileClass,
    /// Which grammar can parse it, if any. `None` is routine and means the file
    /// has no streets (PRD §9), not that anything failed.
    pub language: Option<Language>,
}

impl FileMeta {
    /// A file git has never seen: sized, unplaced in history, ordinary.
    ///
    /// Used for untracked working-tree files, which still get a building.
    pub fn untracked(path: LogicalPath, size_bytes: u64) -> Self {
        Self {
            path,
            size_bytes,
            growth_index: u32::MAX,
            added_at: WallTime::UNIX_EPOCH,
            last_touched: WallTime::UNIX_EPOCH,
            class: FileClass::Ordinary,
            language: None,
        }
    }

    /// True when git has a first-addition commit for this file.
    pub fn is_tracked(&self) -> bool {
        self.growth_index != u32::MAX
    }
}

/// How the landmark layer treats a file (PRD §8).
///
/// > Organic cities are harder to navigate than grids […] The mitigation is the
/// > same one that makes Venice navigable: **landmarks carry the wayfinding, not
/// > addresses.** Invest here. […] Landmarks are the mitigation and must not be
/// > treated as polish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum FileClass {
    /// An ordinary building.
    #[default]
    Ordinary,
    /// An entry point: `main`, `index`, a route table, a CLI root, or a file in
    /// the top decile of inbound imports.
    ///
    /// Rendered tall with a distinct silhouette and **labelled at every zoom**.
    /// These are the orientation anchors — the first thing the eye finds when
    /// zoomed out.
    Monument,
    /// `node_modules`, vendored, generated, `target/`.
    ///
    /// Large, uniform, deliberately dull, drawn as a **single mass** rather than
    /// individual buildings. Making these boring is the feature: the eye should
    /// slide off them.
    Industrial,
    /// Repo root and top-level config — a recognisable open space at the
    /// historic centre.
    CivicSquare,
}

impl FileClass {
    /// True when the file is drawn as part of an undifferentiated mass rather
    /// than as its own building.
    ///
    /// An industrial district gets one polygon, not ten thousand, which is both
    /// the visual intent (PRD §8) and what keeps a `node_modules` tree inside
    /// PRD §13.1's frame budget.
    pub fn is_massed(self) -> bool {
        matches!(self, Self::Industrial)
    }
}

/// A language with a loaded tree-sitter grammar (PRD §9).
///
/// Four grammars ship. A file in any other language is not an error and not a
/// gap in the map: it gets a building like everything else and simply has no
/// streets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Language {
    /// `.rs` — `tree_sitter_rust::LANGUAGE`.
    Rust,
    /// `.js`, `.jsx`, `.mjs`, `.cjs` — `tree_sitter_javascript::LANGUAGE`.
    JavaScript,
    /// `.ts`, `.mts`, `.cts` — `tree_sitter_typescript::LANGUAGE_TYPESCRIPT`.
    TypeScript,
    /// `.tsx` — `tree_sitter_typescript::LANGUAGE_TSX`. A separate grammar, not
    /// a flag on the TypeScript one.
    Tsx,
    /// `.py`, `.pyi` — `tree_sitter_python::LANGUAGE`.
    Python,
}

impl Language {
    /// Every shipped language, in a fixed order.
    pub const ALL: [Self; 5] = [
        Self::Rust,
        Self::JavaScript,
        Self::TypeScript,
        Self::Tsx,
        Self::Python,
    ];

    /// A stable name for logs and for the drill-down panel.
    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::Python => "python",
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// One import, as extracted from source (PRD §9).
///
/// The *edge*, not the street. A street is an aggregate of edges that cross a
/// district boundary ([`imports::Street`]); this is the raw relation, and it is
/// also what PRD §9's "weak attraction force *within* a district" is computed
/// from.
///
/// > **Do not let the import graph fight the directory tree for position.** The
/// > tree determines placement, because directory paths are the addressing
/// > system already in use in every tool call, error message, and conversation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ImportEdge {
    /// The importing file.
    pub from: LogicalPath,
    /// What it imports.
    pub to: ImportTarget,
    /// The module specifier exactly as written in the source — `"./auth"`,
    /// `"crate::auth"`, `"react"`. Kept because resolution is best-effort and
    /// the drill-down panel should be able to show what the code actually said.
    pub specifier: String,
    /// The grammar the edge was extracted from.
    pub language: Language,
}

/// Where an [`ImportEdge`] points.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ImportTarget {
    /// Resolved to a file inside the repository. **Only these can become
    /// streets** — a street is a cross-district relation between two places on
    /// the map, and an external package has no place on it.
    Internal(LogicalPath),
    /// A crate, package or stdlib module outside the repository. Kept rather
    /// than discarded because "this district imports 40 external packages" is a
    /// real diagnostic, but never drawn as a street.
    External,
    /// A specifier the resolver could not classify. Non-fatal by construction:
    /// PRD §9 says a file that fails to parse simply has no streets, and the
    /// same applies to a specifier that fails to resolve.
    Unresolved,
}

impl ImportTarget {
    /// The path, when the target is inside the repository.
    pub fn internal(&self) -> Option<&LogicalPath> {
        match self {
            Self::Internal(p) => Some(p),
            Self::External | Self::Unresolved => None,
        }
    }
}

/// What changed between two index states, so the layout can run one growth step.
///
/// > Growth is genuinely incremental — a new file runs one growth step, it does
/// > not regenerate the world. (PRD §7.4)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDelta {
    /// Files that appeared, in growth order — oldest commit first — so applying
    /// a delta and replaying history from scratch take the same path through the
    /// road-growth algorithm and therefore produce the same city (PRD §7.4).
    pub added: Vec<LogicalPath>,
    /// Files that vanished. They leave vacant lots that go to seed (PRD §7.5),
    /// never holes.
    pub removed: Vec<LogicalPath>,
    /// Files whose size changed enough to change a footprint.
    pub resized: Vec<LogicalPath>,
    /// Files whose last-touched commit moved, which can flip PRD §8 overgrowth.
    pub retouched: Vec<LogicalPath>,
}

impl RepoDelta {
    /// True when nothing changed, so the layout can skip a growth step entirely.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.resized.is_empty()
            && self.retouched.is_empty()
    }

    /// Total number of changed files, for PRD §7.7's rate limiter.
    pub fn len(&self) -> usize {
        self.added.len() + self.removed.len() + self.resized.len() + self.retouched.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    fn tree_with(paths: &[&str]) -> RepoTree {
        let mut tree = RepoTree::default();
        for p in paths {
            let path = lp(p);
            tree.files
                .insert(path.clone(), FileMeta::untracked(path, 1));
        }
        tree
    }

    #[test]
    fn files_in_a_district_are_its_direct_children_only() {
        let tree = tree_with(&["src/a.rs", "src/b.rs", "src/auth/c.rs", "docs/d.md"]);
        let direct: Vec<&str> = tree.files_in(&lp("src")).map(|f| f.path.as_str()).collect();
        assert_eq!(
            direct,
            ["src/a.rs", "src/b.rs"],
            "nested files are not direct"
        );

        let under: Vec<&str> = tree
            .files_under(&lp("src"))
            .map(|f| f.path.as_str())
            .collect();
        assert_eq!(under, ["src/a.rs", "src/auth/c.rs", "src/b.rs"]);
    }

    #[test]
    fn an_untracked_file_is_representable_and_says_so() {
        let f = FileMeta::untracked(lp("scratch.rs"), 120);
        assert!(!f.is_tracked());
        assert_eq!(f.added_at, WallTime::UNIX_EPOCH);
        assert_eq!(f.class, FileClass::Ordinary);
        assert!(
            f.language.is_none(),
            "language is filled in by tree::classify"
        );
    }

    #[test]
    fn a_delta_knows_when_there_is_nothing_to_do() {
        let mut d = RepoDelta::default();
        assert!(d.is_empty());
        assert_eq!(d.len(), 0);
        d.added.push(lp("new.rs"));
        assert!(!d.is_empty());
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn only_internal_import_targets_can_become_streets() {
        let internal = ImportTarget::Internal(lp("src/auth.rs"));
        assert_eq!(
            internal.internal().map(LogicalPath::as_str),
            Some("src/auth.rs")
        );
        assert!(ImportTarget::External.internal().is_none());
        assert!(ImportTarget::Unresolved.internal().is_none());
    }

    #[test]
    fn every_language_has_a_stable_name() {
        let mut names: Vec<&str> = Language::ALL.iter().map(|l| l.name()).collect();
        let n = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), n, "duplicate language name");
        // TSX is its own grammar, not a flag on TypeScript.
        assert_ne!(Language::Tsx, Language::TypeScript);
    }

    #[test]
    fn industrial_is_the_only_massed_class() {
        for c in [
            FileClass::Ordinary,
            FileClass::Monument,
            FileClass::CivicSquare,
        ] {
            assert!(!c.is_massed(), "{c:?}");
        }
        assert!(FileClass::Industrial.is_massed());
    }
}
