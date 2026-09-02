//! Walking the repository, and classifying what is in it (PRD §7.1, §8).
//!
//! This module builds and maintains the [`RepoTree`] every other crate reads.
//! It owns three jobs:
//!
//! 1. **The walk.** Enumerate tracked and untracked files, sized, with the
//!    worktree prefix stripped (PRD §7.6).
//! 2. **Classification.** PRD §8's landmark layer: which files are monuments,
//!    which trees are industrial, what the civic square is.
//! 3. **Incremental refresh.** PRD §7.4 requires growth, not regeneration: a new
//!    commit produces a [`RepoDelta`], and the layout runs one growth step.
//!
//! The directory structure itself — the *districts* of PRD §3, with the per
//! directory aggregates the layout sizes blocks from — is [`DistrictTree`].
//!
//! # Determinism
//!
//! `read_dir` returns filesystem order, which differs between NTFS, ext4 and
//! APFS — and PRD §16 compares golden layout files across two operating systems.
//! **Sort every directory listing.** The tree itself is a `BTreeMap`, so the
//! ordering is recovered at insertion, but anything that consumes the walk in
//! stream order (growth-index assignment, for one) sees `read_dir` order
//! directly. [`walk`] therefore sorts twice: each directory listing before it is
//! pushed, and the whole result before it is returned.
//!
//! Every map and set in this module is a `BTreeMap` or `BTreeSet`, including the
//! intermediate ones that never leave a function. A `HashMap` used "just for
//! counting" is exactly how a non-deterministic tie-break reaches
//! [`monuments`], and a monument that moves between launches destroys the
//! spatial memory PRD §7.4 exists to protect.
//!
//! # Classification is path-only, and that is deliberate
//!
//! [`classify`] takes a [`LogicalPath`] and nothing else, so it can run before
//! any git or import data exists and cannot make the layout depend on ingest
//! timing (PRD §7.4). The one signal it cannot see — "top decile of inbound
//! imports", one of PRD §8's monument criteria — is applied afterwards by
//! [`promote_monuments`], from data [`crate::imports`] produced.
//!
//! # This module never calls git
//!
//! [`RepoIndex`] produces a complete, classified, laid-out-able tree from the
//! filesystem alone; every history field stays at its [`FileMeta::untracked`]
//! default until a caller supplies git's answer through [`RepoIndex::apply_growth`]
//! and [`RepoIndex::apply_last_touched`]. Those two take plain slices rather than
//! [`crate::git`] types on purpose: the growth sequence is expensive, cached, and
//! sometimes absent (a directory that is not a git repository still gets a city),
//! and a tree that cannot be built without it would make that impossible.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use polis_events::{LogicalPath, WallTime, WorktreeId};
use serde::{Deserialize, Serialize};

use crate::{FileClass, FileMeta, ImportEdge, Language, RepoDelta, RepoTree};

// ---------------------------------------------------------------------------
// The index
// ---------------------------------------------------------------------------

/// Builds and incrementally maintains a [`RepoTree`].
#[derive(Debug)]
pub struct RepoIndex {
    tree: RepoTree,
    options: WalkOptions,
    growth_cache: Option<PathBuf>,
    next_worktree: u32,
}

impl RepoIndex {
    /// Indexes a repository from scratch.
    ///
    /// PRD §13.1 budgets cold start → first frame at under 3 s for a 5 000-file
    /// repo, and this is most of it, so the git walk is cached (see
    /// [`crate::git::GrowthSequence`]).
    ///
    /// The returned tree is complete and classified but has **no history**:
    /// every [`FileMeta`] reports [`FileMeta::is_tracked`] as `false` until
    /// [`RepoIndex::apply_growth`] is called. [`RepoTree::head`] is empty for the
    /// same reason.
    pub fn open(root: &Path) -> anyhow::Result<Self> {
        Self::open_with(root, WalkOptions::default())
    }

    /// [`RepoIndex::open`] with a non-default walk.
    ///
    /// The usual reason is [`WalkOptions::skip_massed`]: enumerating a
    /// `node_modules` with 40 000 files inside it costs more than PRD §13.1's
    /// entire cold-start budget, and PRD §8 draws that tree as a single dull
    /// mass either way.
    pub fn open_with(root: &Path, options: WalkOptions) -> anyhow::Result<Self> {
        let files = walk_with(root, &options)?;
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
        Ok(Self {
            tree,
            options,
            growth_cache: None,
            next_worktree: 1,
        })
    }

    /// Indexes a repository, reusing a cached growth sequence when its `HEAD`
    /// still matches.
    ///
    /// The cache belongs to [`crate::git::GrowthSequence`], which owns both
    /// halves of reading and writing it; this records the path so a caller
    /// holding an index does not have to thread it separately, and is otherwise
    /// [`RepoIndex::open`]. Read it back with [`RepoIndex::growth_cache`].
    pub fn open_with_cache(root: &Path, cache: &Path) -> anyhow::Result<Self> {
        let mut index = Self::open(root)?;
        index.growth_cache = Some(cache.to_path_buf());
        Ok(index)
    }

    /// The current tree.
    pub fn tree(&self) -> &RepoTree {
        &self.tree
    }

    /// The path the growth-sequence cache was opened with, if any.
    pub fn growth_cache(&self) -> Option<&Path> {
        self.growth_cache.as_deref()
    }

    /// The district tree over the current files.
    ///
    /// Derived, not stored: it is cheap (one pass, `O(files x depth)`) and a
    /// stored copy is one more thing that can disagree with [`RepoIndex::tree`]
    /// after a refresh.
    pub fn districts(&self) -> DistrictTree {
        DistrictTree::from_repo(&self.tree, self.options.industrial())
    }

    /// Applies new commits and working-tree changes without recomputing the
    /// world.
    ///
    /// > Growth is genuinely incremental — a new file runs one growth step, it
    /// > does not regenerate the world. (PRD §7.4)
    ///
    /// [`RepoDelta::added`] must come back in growth order, so that applying a
    /// delta and replaying history from scratch take the same path through the
    /// road-growth algorithm. A file that appeared in the working tree since the
    /// last walk has no commit and therefore no growth index, so within one
    /// refresh the order is by logical path — deterministic, and identical on
    /// every machine, which is the property that actually matters. Once
    /// [`RepoIndex::apply_growth`] has seen the commit that added them they sort
    /// by growth index like everything else.
    ///
    /// [`RepoDelta::retouched`] is always empty here: "last touched" is a commit
    /// time, and only [`RepoIndex::apply_last_touched`] knows it.
    pub fn refresh(&mut self) -> anyhow::Result<RepoDelta> {
        let walked = walk_with(&self.tree.root, &self.options)?;
        let mut next: BTreeMap<LogicalPath, FileMeta> = BTreeMap::new();
        let mut delta = RepoDelta::default();

        for mut meta in walked {
            match self.tree.files.get(&meta.path) {
                None => delta.added.push(meta.path.clone()),
                Some(old) => {
                    if old.size_bytes != meta.size_bytes {
                        delta.resized.push(meta.path.clone());
                    }
                    // History is git's, and the walk knows nothing about it.
                    meta.growth_index = old.growth_index;
                    meta.added_at = old.added_at;
                    meta.last_touched = old.last_touched;
                    // A file that was promoted to a monument by the import graph
                    // keeps that promotion; the path-only classification cannot
                    // rediscover it.
                    if old.class == FileClass::Monument && meta.class == FileClass::Ordinary {
                        meta.class = FileClass::Monument;
                    }
                }
            }
            next.insert(meta.path.clone(), meta);
        }
        for path in self.tree.files.keys() {
            if !next.contains_key(path) {
                delta.removed.push(path.clone());
            }
        }

        self.tree.files = next;
        Ok(delta)
    }

    /// Registers a worktree discovered at runtime (PRD §7.6).
    ///
    /// Adds no files and no geometry: a worktree is a tint over the one shared
    /// base map, never a second city.
    ///
    /// Ids are handed out in registration order, so replaying the same sequence
    /// of discoveries yields the same ids. Registering a path twice returns the
    /// id it already has rather than minting a second one.
    pub fn add_worktree(&mut self, root: &Path) -> anyhow::Result<WorktreeId> {
        if let Some((id, _)) = self
            .tree
            .worktrees
            .iter()
            .find(|(_, p)| p.as_path() == root)
        {
            return Ok(*id);
        }
        let id = WorktreeId(self.next_worktree);
        self.next_worktree += 1;
        self.tree.worktrees.insert(id, root.to_path_buf());
        Ok(id)
    }

    /// Folds git's growth order into the tree (PRD §7.1).
    ///
    /// `entries` is `(path, commit time)` in commit order, oldest first — the
    /// shape [`crate::git::GrowthSequence::entries`] has. The index into that
    /// slice becomes [`FileMeta::growth_index`], so the *slice position*, not the
    /// timestamp, is what orders the city; two commits with the same `%ct` (a
    /// rebase, a squash, a clock that went backwards) still have a total order.
    ///
    /// Paths git knows about but the walk did not see are ignored: they are
    /// deleted files, and PRD §7.5 gives those a vacant lot, not a building.
    /// Returns how many files were matched.
    pub fn apply_growth(&mut self, entries: &[(LogicalPath, WallTime)]) -> usize {
        let mut matched = 0;
        for (index, (path, at)) in entries.iter().enumerate() {
            if let Some(meta) = self.tree.files.get_mut(path) {
                meta.growth_index = u32::try_from(index).unwrap_or(u32::MAX - 1);
                meta.added_at = *at;
                // Not every caller supplies `last_touched`, and a file whose
                // last touch is its first commit is the common case.
                if meta.last_touched == WallTime::UNIX_EPOCH {
                    meta.last_touched = *at;
                }
                matched += 1;
            }
        }
        matched
    }

    /// Folds git's per-file last-commit times into the tree (PRD §8's
    /// overgrowth). Returns how many files were matched.
    pub fn apply_last_touched(&mut self, entries: &[(LogicalPath, WallTime)]) -> usize {
        let mut matched = 0;
        for (path, at) in entries {
            if let Some(meta) = self.tree.files.get_mut(path) {
                meta.last_touched = *at;
                matched += 1;
            }
        }
        matched
    }

    /// Records the `HEAD` the tree is indexed at, which every cache keyed on it
    /// compares against.
    pub fn set_head(&mut self, head: impl Into<String>) {
        self.tree.head = head.into();
    }
}

// ---------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------

/// How [`walk_with`] enumerates a checkout.
///
/// The default is the faithful walk: everything, `node_modules` included,
/// because PRD §8 draws that tree rather than omitting it.
#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
    /// Do not descend into industrial trees, and emit no files for them.
    ///
    /// `false` by default, matching PRD §8: `node_modules` is *drawn*, as one
    /// dull mass, rather than omitted. Set it when the budget matters more than
    /// the mass being correctly sized — the district still exists, it just has
    /// no files under it.
    pub skip_massed: bool,
    /// Follow symlinks, of files as well as directories.
    ///
    /// `false` by default, which skips them entirely rather than treating them
    /// as ordinary files. A symlink loop is an infinite walk, and a symlinked
    /// tree is the same logical file in two physical places — precisely the
    /// duplication PRD §7.6 spends a section avoiding, arriving by a different
    /// route. A `pnpm` workspace is the common case and it is `node_modules`
    /// either way.
    pub follow_symlinks: bool,
    /// Stop after this many files. `None` — the default — walks everything.
    ///
    /// A safety valve for a pathological tree, not a normal setting: the walk
    /// returns what it has, which is a *smaller city*, not an error.
    pub max_files: Option<usize>,
    /// Which trees are industrial. `None` uses [`default_industrial_rules`].
    pub industrial_rules: Option<IndustrialRules>,
    /// Paths the walk must not enter because Polis writes them.
    ///
    /// `None` uses [`default_walk_exclusions`]. Set it to
    /// [`WalkExclusions::empty`] for a deliberately faithful walk, and to
    /// [`WalkExclusions::shipped`] plus [`WalkExclusions::exclude_output`] for
    /// anything that is about to write a file into the checkout.
    pub exclusions: Option<WalkExclusions>,
}

impl WalkOptions {
    /// The rules in force, shipped defaults included.
    pub fn industrial(&self) -> &IndustrialRules {
        self.industrial_rules
            .as_ref()
            .unwrap_or_else(|| default_industrial_rules())
    }

    /// The exclusions in force, shipped defaults included.
    pub fn excluded(&self) -> &WalkExclusions {
        self.exclusions
            .as_ref()
            .unwrap_or_else(|| default_walk_exclusions())
    }
}

/// Directory names never walked, whatever the options say.
///
/// `.git` is not a district. It is not in `git ls-files`, it holds no source,
/// and on a busy repository it is tens of thousands of loose objects — a walk
/// that descends into it spends its entire budget there and renders a city made
/// of hashes.
const NEVER_WALKED: &[&str] = &[".git"];

// ---------------------------------------------------------------------------
// The repo-walk trap
// ---------------------------------------------------------------------------

/// Paths the walk does not enter, because **Polis itself writes them**.
///
/// # The trap this closes
///
/// The city-layout design bake-off rendered its candidate cities into
/// `docs/design/`. `docs/design/` is inside the repository. So every run added a
/// few files to the repository the *next* run laid out, and the town grew by
/// itself between two runs that were supposed to be identical. Nothing errored,
/// no test failed, and the only symptom was a layout that drifted — the single
/// hardest class of bug to diagnose in a system whose entire product promise
/// (PRD §7.4) is that the map does not move.
///
/// It is a feedback loop, not a classification problem, and it is closed by
/// refusing to ingest the tool's own output.
///
/// # Why this is not [`IndustrialRules`]
///
/// The two lists look alike and mean opposite things. PRD §8 requires
/// `node_modules` to be **drawn**, as one dull mass — it is part of the
/// repository and the operator should see how much of it there is. An excluded
/// path is not drawn at all, because it is not part of the repository in any
/// sense the operator cares about: it exists because Polis ran.
///
/// So `dist/` is industrial and `docs/design/` is excluded, and merging the two
/// lists would either start drawing the tool's own PNGs as buildings or stop
/// drawing the dependency tree PRD §8 asks for.
///
/// # Configurable, and why that is not optional
///
/// A hardcoded list is wrong in both directions here. Every project puts its
/// generated artefacts somewhere different, and the shipped list will always be
/// missing one — while a repository whose `docs/design/` is hand-written prose
/// would have a real district silently vanish with no way to say otherwise.
///
/// Two rules, either of which is enough:
///
/// * **`dir_names`** — a directory of this name at any depth.
/// * **`prefixes`** — a root-anchored path. A directory *or a single file*:
///   `LogicalPath::starts_with` matches an exact path as well as a subtree,
///   which is what lets [`WalkExclusions::exclude_output`] name one PNG rather
///   than the whole of `docs/`.
///
/// Matching is ASCII case-insensitive, like [`LogicalPath`]'s own equality
/// (ADR-0028).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkExclusions {
    /// Directory names, lowercased, matched at any depth.
    dir_names: BTreeSet<String>,
    /// Root-anchored paths: a subtree, or one file.
    prefixes: BTreeSet<LogicalPath>,
}

/// Root-anchored paths excluded by default.
///
/// One entry, and it is the one the bake-off was actually caught by. The general
/// closure is [`WalkExclusions::exclude_output`], which every writer of a file
/// into the repository is expected to call: a shipped list can only ever name
/// yesterday's artefact directory.
const DEFAULT_EXCLUDED_PREFIXES: &[&str] = &["docs/design"];

/// Directory names excluded by default, at any depth.
///
/// Polis's own state and cache directories. Deliberately short: `target/`,
/// `dist/` and `node_modules/` are **not** here, because PRD §8 draws them (see
/// the type documentation).
const DEFAULT_EXCLUDED_DIRS: &[&str] = &[".polis", ".polis-cache"];

/// The shipped exclusions, built once.
///
/// A `OnceLock` for the same reason [`default_industrial_rules`] is one: the
/// walk tests every directory it meets against these.
pub fn default_walk_exclusions() -> &'static WalkExclusions {
    static RULES: OnceLock<WalkExclusions> = OnceLock::new();
    RULES.get_or_init(|| {
        let mut rules = WalkExclusions::empty();
        for name in DEFAULT_EXCLUDED_DIRS {
            rules.push_dir(name);
        }
        for path in DEFAULT_EXCLUDED_PREFIXES {
            rules.push_path(path);
        }
        rules
    })
}

impl WalkExclusions {
    /// Nothing is excluded.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            dir_names: BTreeSet::new(),
            prefixes: BTreeSet::new(),
        }
    }

    /// The shipped defaults.
    #[must_use]
    pub fn shipped() -> Self {
        default_walk_exclusions().clone()
    }

    /// Excludes a directory name, matched at any depth.
    pub fn push_dir(&mut self, name: &str) {
        let name = name.trim().trim_matches('/');
        if !name.is_empty() {
            self.dir_names.insert(name.to_ascii_lowercase());
        }
    }

    /// Excludes a root-anchored path: a subtree, or one file.
    ///
    /// An unparseable path, or the repository root itself, is ignored — a walk
    /// that refused to enter the repository would be a worse bug than the one
    /// this prevents.
    pub fn push_path(&mut self, path: &str) {
        if let Ok(path) = LogicalPath::new(path) {
            if !path.is_root() {
                self.prefixes.insert(path);
            }
        }
    }

    /// Stops excluding a directory name — the way to say "our `docs/design` is
    /// hand-written".
    pub fn remove_dir(&mut self, name: &str) -> bool {
        self.dir_names.remove(&name.trim().to_ascii_lowercase())
    }

    /// Excludes a file Polis is about to write, if it lands inside `repo_root`.
    ///
    /// **This is the general form of the fix**, and every path Polis writes
    /// should go through it. A shipped list of artefact directories can only
    /// name the ones that existed when it was written; "do not ingest what you
    /// are about to emit" holds for the next one too.
    ///
    /// A path outside the repository is not excluded, because it cannot be
    /// walked. Returns whether anything was added, so a caller can log it.
    pub fn exclude_output(&mut self, output: &Path, repo_root: &Path) -> bool {
        let resolved = output.canonicalize().or_else(|_| {
            // The file usually does not exist yet, so canonicalise its parent
            // and re-attach the name.
            let parent = output.parent().filter(|p| !p.as_os_str().is_empty());
            let parent = parent.unwrap_or(Path::new("."));
            parent
                .canonicalize()
                .map(|p| output.file_name().map_or_else(|| p.clone(), |n| p.join(n)))
        });
        let (Ok(out), Ok(root)) = (resolved, repo_root.canonicalize()) else {
            return false;
        };
        let Ok(rel) = out.strip_prefix(&root) else {
            return false;
        };
        let text: String = rel
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .collect::<Vec<_>>()
            .join("/");
        let before = self.prefixes.len();
        self.push_path(&text);
        self.prefixes.len() > before
    }

    /// True for a **directory** the walk must not enter.
    #[must_use]
    pub fn excludes_dir(&self, dir: &LogicalPath) -> bool {
        if self.matches_prefix(dir) {
            return true;
        }
        dir.components()
            .any(|c| self.dir_names.contains(&c.to_ascii_lowercase()))
    }

    /// True for a **file** the walk must not emit.
    ///
    /// The file's own name is never tested against `dir_names`: a file called
    /// `.polis` at the root is a config file and gets a building.
    #[must_use]
    pub fn excludes_file(&self, file: &LogicalPath) -> bool {
        if self.matches_prefix(file) {
            return true;
        }
        let mut components: Vec<&str> = file.components().collect();
        components.pop();
        components
            .into_iter()
            .any(|c| self.dir_names.contains(&c.to_ascii_lowercase()))
    }

    fn matches_prefix(&self, path: &LogicalPath) -> bool {
        self.prefixes.iter().any(|p| path.starts_with(p))
    }

    /// How many rules are configured, of both kinds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.dir_names.len() + self.prefixes.len()
    }

    /// True when nothing is excluded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Walks a checkout and returns one [`FileMeta`] per file, **sorted by logical
/// path**.
///
/// Sizes come from the filesystem; every history field is left at its
/// [`FileMeta::untracked`] default for [`crate::git`] to fill in. Excluded trees
/// are still walked — PRD §8 renders `node_modules` as a single dull mass rather
/// than omitting it — but see [`is_massed_tree`] for how they are collapsed, and
/// [`WalkOptions::skip_massed`] for the escape hatch when the cold-start budget
/// matters more.
pub fn walk(root: &Path) -> std::io::Result<Vec<FileMeta>> {
    walk_with(root, &WalkOptions::default())
}

/// [`walk`], with the enumeration rules spelled out.
///
/// Every directory listing is sorted before it is used and the result is sorted
/// again before it is returned, so the output is byte-identical on NTFS, ext4 and
/// APFS (PRD §7.4, §16).
///
/// Two things are skipped silently, both of which are normal rather than
/// exceptional: a name that is not valid UTF-8 (ADR-0028 rejects those rather
/// than lossily converting two different files onto one key), and an entry whose
/// metadata cannot be read because it vanished mid-walk.
pub fn walk_with(root: &Path, opts: &WalkOptions) -> std::io::Result<Vec<FileMeta>> {
    let rules = opts.industrial();
    let excluded = opts.excluded();
    let mut out: Vec<FileMeta> = Vec::new();
    // (physical directory, its logical path). A stack, not recursion: a deep
    // node_modules will happily blow a recursive walk's stack.
    let mut stack = vec![(root.to_path_buf(), LogicalPath::root())];

    while let Some((dir, logical)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // The root not existing is the caller's problem; a subdirectory
            // that vanished or is unreadable mid-walk is not.
            Err(e) if dir == root => return Err(e),
            Err(e) => {
                tracing::debug!(dir = %dir.display(), error = %e, "unreadable directory, skipped");
                continue;
            }
        };

        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() && !opts.follow_symlinks {
                continue;
            }
            let Ok(child) = logical.join(&name) else {
                continue;
            };
            // `file_type` does not follow symlinks; `metadata` does, which is
            // what makes a followed symlink resolve to its target's kind.
            let is_dir = if file_type.is_symlink() {
                std::fs::metadata(entry.path()).is_ok_and(|m| m.is_dir())
            } else {
                file_type.is_dir()
            };
            if is_dir {
                if NEVER_WALKED.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                    continue;
                }
                if opts.skip_massed && rules.is_industrial_dir(&child) {
                    continue;
                }
                // The repo-walk trap: a directory Polis writes into is not a
                // district. See [`WalkExclusions`].
                if excluded.excludes_dir(&child) {
                    continue;
                }
                dirs.push((entry.path(), child));
            } else {
                if excluded.excludes_file(&child) {
                    continue;
                }
                let size = entry.metadata().map_or(0, |m| m.len());
                files.push((child, size));
            }
        }

        files.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, size) in files {
            if opts.max_files.is_some_and(|max| out.len() >= max) {
                out.sort_by(|a, b| a.path.cmp(&b.path));
                return Ok(out);
            }
            let mut meta = FileMeta::untracked(path.clone(), size);
            meta.class = classify_with(&path, rules);
            meta.language = language_for(&path);
            out.push(meta);
        }

        // Sorted, then reversed: the stack pops the last element, so this walks
        // subdirectories in ascending name order.
        dirs.sort_by(|a, b| a.1.cmp(&b.1));
        dirs.reverse();
        stack.extend(dirs);
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

// ---------------------------------------------------------------------------
// Districts
// ---------------------------------------------------------------------------

/// The directory tree, with the aggregates the layout needs per district
/// (PRD §3, §7.2, §8).
///
/// Districts are directories, and *the directory tree determines placement*
/// (PRD §9) — so this, not the import graph, is the structure the layout engine
/// subdivides. Keyed and iterated in [`LogicalPath`] order; the root district is
/// always present, even for an empty repository.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DistrictTree {
    districts: BTreeMap<LogicalPath, District>,
}

/// One directory, and everything under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct District {
    /// The directory's logical path. [`LogicalPath::root`] for the civic square.
    pub path: LogicalPath,
    /// How PRD §8 renders it.
    pub kind: DistrictKind,
    /// Files anywhere below, at any depth.
    pub file_count: u32,
    /// Files directly inside, which is what determines how many lots this
    /// district's own blocks need.
    pub direct_file_count: u32,
    /// Total size of every file below, at any depth. Footprint area is
    /// proportional to `sqrt(size)` per building (PRD §7.3); this is the
    /// district-level equivalent and is what a collapsed industrial mass is
    /// sized from.
    pub total_size_bytes: u64,
    /// How far the deepest file below this district is, in path components.
    /// `1` for a district whose files are all direct children, `0` for one with
    /// no files at all.
    pub max_depth: u32,
    /// Immediate child directories, in path order.
    pub children: BTreeSet<LogicalPath>,
}

/// How PRD §8 renders a district.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub enum DistrictKind {
    /// An ordinary district: blocks, lots, buildings.
    #[default]
    Normal,
    /// `node_modules`, vendored, generated, `target/`. Drawn as one mass.
    Industrial,
    /// The repository root — "a recognisable open space at the historic centre".
    CivicSquare,
}

impl DistrictKind {
    /// Which kind a directory is, under a given rule set.
    pub fn of(path: &LogicalPath, rules: &IndustrialRules) -> Self {
        if path.is_root() {
            Self::CivicSquare
        } else if rules.is_industrial_dir(path) {
            Self::Industrial
        } else {
            Self::Normal
        }
    }
}

impl District {
    fn new(path: LogicalPath, rules: &IndustrialRules) -> Self {
        Self {
            kind: DistrictKind::of(&path, rules),
            path,
            file_count: 0,
            direct_file_count: 0,
            total_size_bytes: 0,
            max_depth: 0,
            children: BTreeSet::new(),
        }
    }

    /// True when this district is drawn as one undifferentiated mass rather than
    /// as individual buildings.
    pub fn is_massed(&self) -> bool {
        self.kind == DistrictKind::Industrial
    }
}

impl DistrictTree {
    /// Builds the district tree over a [`RepoTree`]'s files.
    pub fn from_repo(tree: &RepoTree, rules: &IndustrialRules) -> Self {
        Self::from_files(tree.files.values(), rules)
    }

    /// Builds the district tree over any set of files.
    ///
    /// The paths must already be logical — worktree-stripped, PRD §7.6. Feeding
    /// this two worktrees' physical paths produces two half-empty cities, which
    /// is the exact failure §7.6 says to decide against now rather than retrofit
    /// later; run them through `polis_events::PathMapper` first and they collapse
    /// onto one district tree with the file counts summed correctly.
    pub fn from_files<'a, I>(files: I, rules: &IndustrialRules) -> Self
    where
        I: IntoIterator<Item = &'a FileMeta>,
    {
        let mut districts: BTreeMap<LogicalPath, District> = BTreeMap::new();
        districts.insert(
            LogicalPath::root(),
            District::new(LogicalPath::root(), rules),
        );

        for meta in files {
            let parent = meta.path.parent().unwrap_or_else(LogicalPath::root);
            // The parent, then every ancestor, ending at the root.
            let mut chain = Vec::new();
            let mut cursor = Some(parent);
            while let Some(current) = cursor {
                let is_root = current.is_root();
                cursor = if is_root { None } else { current.parent() };
                chain.push(current);
            }

            let depth = meta.path.depth();
            for (rank, district) in chain.iter().enumerate() {
                let entry = districts
                    .entry(district.clone())
                    .or_insert_with(|| District::new(district.clone(), rules));
                entry.file_count = entry.file_count.saturating_add(1);
                entry.total_size_bytes = entry.total_size_bytes.saturating_add(meta.size_bytes);
                let below =
                    u32::try_from(depth.saturating_sub(district.depth())).unwrap_or(u32::MAX);
                entry.max_depth = entry.max_depth.max(below);
                if rank == 0 {
                    entry.direct_file_count = entry.direct_file_count.saturating_add(1);
                }
            }
            for pair in chain.windows(2) {
                if let Some(parent) = districts.get_mut(&pair[1]) {
                    parent.children.insert(pair[0].clone());
                }
            }
        }

        Self { districts }
    }

    /// One district by path.
    pub fn district(&self, path: &LogicalPath) -> Option<&District> {
        self.districts.get(path)
    }

    /// The root district — the civic square. Always present.
    pub fn root(&self) -> &District {
        self.districts
            .get(&LogicalPath::root())
            .expect("the root district is inserted at construction")
    }

    /// Every district, in path order.
    pub fn districts(&self) -> impl Iterator<Item = &District> + '_ {
        self.districts.values()
    }

    /// The immediate child districts of one district, in path order.
    pub fn children<'a>(&'a self, path: &LogicalPath) -> impl Iterator<Item = &'a District> + 'a {
        self.districts
            .get(path)
            .into_iter()
            .flat_map(move |d| d.children.iter().filter_map(move |c| self.districts.get(c)))
    }

    /// How many districts there are, the root included.
    pub fn len(&self) -> usize {
        self.districts.len()
    }

    /// True when the tree has nothing but its root.
    pub fn is_empty(&self) -> bool {
        self.districts.len() <= 1 && self.root().file_count == 0
    }
}

// ---------------------------------------------------------------------------
// Industrial rules
// ---------------------------------------------------------------------------

/// Which trees are rendered as an undifferentiated mass (PRD §8).
///
/// Configurable rather than a hardcoded list, for two reasons that show up
/// immediately in real repositories. Every ecosystem names its build directory
/// differently and the shipped list will always be missing one. And the opposite
/// case is worse: a repository with a first-class `dist/` or `generated/`
/// directory full of code people actually read would have it silently flattened
/// into scenery, with no way to say otherwise.
///
/// Three independent rules, any of which is enough:
///
/// * **`dir_names`** — a directory with this name, at any depth. `node_modules`,
///   `target`, `dist`.
/// * **`prefixes`** — a specific subtree, root-anchored. `docs/api/generated`.
/// * **`generated_suffixes`** — a file name ending. `.min.js`, `_pb2.py`. This
///   one is per file rather than per tree, because generated files are usually
///   mixed in with hand-written ones rather than kept apart.
///
/// All matching is ASCII case-insensitive, matching [`LogicalPath`]'s own
/// equality (ADR-0028).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndustrialRules {
    /// Directory names, lowercased, matched at any depth.
    dir_names: BTreeSet<String>,
    /// Root-anchored subtrees.
    prefixes: BTreeSet<LogicalPath>,
    /// File-name endings, lowercased.
    generated_suffixes: BTreeSet<String>,
}

/// Directory names that are a build or dependency tree in some ecosystem.
///
/// Deliberately excludes names that are as often source as not: `bin` (Rust's
/// `src/bin` is where binaries' entry points live), `lib`, `gen`, `assets`,
/// `public`, `static`. A false positive here is worse than a false negative,
/// because it takes real code and makes it scenery the eye is trained to slide
/// off.
const DEFAULT_INDUSTRIAL_DIRS: &[&str] = &[
    // JavaScript / TypeScript
    "node_modules",
    "bower_components",
    "jspm_packages",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".output",
    ".turbo",
    ".parcel-cache",
    ".angular",
    // Rust / Go / JVM / .NET / C++
    "target",
    "obj",
    ".gradle",
    "cmake-build-debug",
    "cmake-build-release",
    // Python
    "__pycache__",
    ".venv",
    "venv",
    "site-packages",
    ".eggs",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    // Dart / Elm / iOS
    ".dart_tool",
    "elm-stuff",
    "pods",
    "deriveddata",
    // Generic build output and vendored code
    "dist",
    "build",
    "_build",
    "out",
    "vendor",
    "third_party",
    "generated",
    "coverage",
    "htmlcov",
    ".nyc_output",
    ".terraform",
    ".cache",
    ".git",
];

/// File-name endings that mean "a tool wrote this".
const DEFAULT_GENERATED_SUFFIXES: &[&str] = &[
    ".min.js",
    ".min.css",
    ".js.map",
    ".css.map",
    ".pb.go",
    ".pb.cc",
    ".pb.h",
    "_pb2.py",
    "_pb2_grpc.py",
    ".g.dart",
    ".freezed.dart",
    ".generated.ts",
    ".generated.rs",
    ".designer.cs",
];

/// The shipped rules, built once.
///
/// A `OnceLock` rather than a fresh [`IndustrialRules::default`] per call:
/// [`classify`] is called once per file during the walk, and allocating forty
/// `String`s into a `BTreeSet` per file is most of a cold start on a large
/// repository.
pub fn default_industrial_rules() -> &'static IndustrialRules {
    static RULES: OnceLock<IndustrialRules> = OnceLock::new();
    RULES.get_or_init(|| {
        let mut rules = IndustrialRules::empty();
        for name in DEFAULT_INDUSTRIAL_DIRS {
            rules.push_dir(name);
        }
        for suffix in DEFAULT_GENERATED_SUFFIXES {
            rules.push_generated_suffix(suffix);
        }
        rules
    })
}

impl Default for IndustrialRules {
    fn default() -> Self {
        default_industrial_rules().clone()
    }
}

impl IndustrialRules {
    /// No rule at all: nothing is industrial.
    pub fn empty() -> Self {
        Self {
            dir_names: BTreeSet::new(),
            prefixes: BTreeSet::new(),
            generated_suffixes: BTreeSet::new(),
        }
    }

    /// Adds a directory name, matched at any depth.
    pub fn push_dir(&mut self, name: &str) {
        let name = name.trim().trim_matches('/');
        if !name.is_empty() {
            self.dir_names.insert(name.to_ascii_lowercase());
        }
    }

    /// Adds a root-anchored subtree. An unparseable path is ignored.
    pub fn push_prefix(&mut self, path: &str) {
        if let Ok(path) = LogicalPath::new(path) {
            if !path.is_root() {
                self.prefixes.insert(path);
            }
        }
    }

    /// Adds a generated-file name ending.
    pub fn push_generated_suffix(&mut self, suffix: &str) {
        let suffix = suffix.trim();
        if !suffix.is_empty() {
            self.generated_suffixes.insert(suffix.to_ascii_lowercase());
        }
    }

    /// Removes a directory name — the way to say "our `dist/` is real code".
    pub fn remove_dir(&mut self, name: &str) -> bool {
        self.dir_names.remove(&name.trim().to_ascii_lowercase())
    }

    /// True for a **directory** that is industrial: its own name counts, which
    /// is what makes `node_modules` itself industrial and not merely everything
    /// underneath it.
    pub fn is_industrial_dir(&self, dir: &LogicalPath) -> bool {
        if self.matches_prefix(dir) {
            return true;
        }
        dir.components().any(|c| self.is_industrial_name(c))
    }

    /// True for a **file** that is industrial: any *ancestor* directory is
    /// industrial, or the file name itself looks generated.
    ///
    /// The file's own name is not tested against `dir_names`, so a shell script
    /// called `build` at the repository root stays a building.
    pub fn is_industrial_file(&self, file: &LogicalPath) -> bool {
        if self.matches_prefix(file) {
            return true;
        }
        if let Some(name) = file.file_name() {
            let lower = name.to_ascii_lowercase();
            if self
                .generated_suffixes
                .iter()
                .any(|s| lower.ends_with(s.as_str()))
            {
                return true;
            }
        }
        let mut components: Vec<&str> = file.components().collect();
        components.pop();
        components.into_iter().any(|c| self.is_industrial_name(c))
    }

    fn is_industrial_name(&self, component: &str) -> bool {
        self.dir_names.contains(&component.to_ascii_lowercase())
    }

    fn matches_prefix(&self, path: &LogicalPath) -> bool {
        self.prefixes.iter().any(|p| path.starts_with(p))
    }

    /// How many rules are configured, of all three kinds.
    pub fn len(&self) -> usize {
        self.dir_names.len() + self.prefixes.len() + self.generated_suffixes.len()
    }

    /// True when nothing is industrial.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// True for a directory that is rendered as one undifferentiated mass rather
/// than as individual buildings (PRD §8).
///
/// `node_modules`, vendored and generated trees, `target/`. This is the
/// *rendering* decision; `polis_ingest::fswatch::is_watch_excluded` is the
/// separate decision about whether to watch them at all.
///
/// Uses the shipped [`default_industrial_rules`]; call
/// [`IndustrialRules::is_industrial_dir`] directly to apply an operator's own.
pub fn is_massed_tree(path: &LogicalPath) -> bool {
    default_industrial_rules().is_industrial_dir(path)
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// Classifies a file from its path alone (PRD §8).
///
/// Deterministic and path-only, so it can run before any git or import data
/// exists and cannot make the layout depend on ingest timing (PRD §7.4). The
/// inbound-import criterion for a monument is applied separately, by
/// [`promote_monuments`].
pub fn classify(path: &LogicalPath) -> FileClass {
    classify_with(path, default_industrial_rules())
}

/// [`classify`] under an operator's own industrial rules.
///
/// Precedence, and why it is this way:
///
/// 1. **Industrial** wins over everything. `node_modules` contains ten thousand
///    files called `index.js`, and if entry points were checked first the
///    wayfinding layer would be entirely made of other people's packages.
/// 2. **Monument** next. A top-level `index.ts` is an entry point first and a
///    root-directory file second.
/// 3. **Civic square** for the repository root and top-level config.
/// 4. Everything else is ordinary.
pub fn classify_with(path: &LogicalPath, rules: &IndustrialRules) -> FileClass {
    if path.is_root() {
        return FileClass::CivicSquare;
    }
    if rules.is_industrial_file(path) {
        return FileClass::Industrial;
    }
    if entry_point(path).is_some() {
        return FileClass::Monument;
    }
    if is_civic(path) {
        return FileClass::CivicSquare;
    }
    FileClass::Ordinary
}

/// Which grammar can parse a file, by extension (PRD §9).
///
/// `None` means no grammar is loaded for that language, which is a normal
/// condition: that file has no streets and nothing else changes. Note that
/// `.tsx` is [`Language::Tsx`], a **separate grammar**, not a flag on
/// [`Language::TypeScript`].
pub fn language_for(path: &LogicalPath) -> Option<Language> {
    let ext = path.extension()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => Language::Rust,
        "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
        "ts" | "mts" | "cts" => Language::TypeScript,
        "tsx" => Language::Tsx,
        "py" | "pyi" => Language::Python,
        _ => return None,
    })
}

/// True for the repository root and for top-level configuration — PRD §8's
/// "recognisable open space at the historic centre".
///
/// Depth-one only, deliberately. `src/auth/config.toml` is a file an agent works
/// on; `Cargo.toml` is a file every agent glances at. Only the second is a civic
/// square, and the difference is exactly the depth.
pub fn is_civic(path: &LogicalPath) -> bool {
    if path.is_root() {
        return true;
    }
    if path.depth() != 1 {
        return false;
    }
    let Some(name) = path.file_name() else {
        return false;
    };
    // A dotfile at the root is configuration by convention: .gitignore,
    // .editorconfig, .env.example, .nvmrc.
    if name.starts_with('.') {
        return true;
    }
    if CIVIC_NAMES.iter().any(|n| n.eq_ignore_ascii_case(name)) {
        return true;
    }
    let stem = name.split_once('.').map_or(name, |(stem, _)| stem);
    if CIVIC_STEMS.iter().any(|s| s.eq_ignore_ascii_case(stem)) {
        return true;
    }
    path.extension()
        .is_some_and(|ext| CIVIC_EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext)))
}

/// Extensionless or oddly-named top-level files that are still config.
const CIVIC_NAMES: &[&str] = &[
    "Makefile",
    "Dockerfile",
    "Jenkinsfile",
    "Rakefile",
    "Gemfile",
    "Procfile",
    "justfile",
    "Vagrantfile",
    "CMakeLists.txt",
];

/// Top-level file *stems* that are orientation documents whatever their
/// extension: `README`, `README.md`, `README.rst`.
const CIVIC_STEMS: &[&str] = &[
    "README",
    "LICENSE",
    "LICENCE",
    "COPYING",
    "NOTICE",
    "AUTHORS",
    "CHANGELOG",
    "CONTRIBUTING",
    "CODE_OF_CONDUCT",
    "SECURITY",
    "CLAUDE",
    "AGENTS",
];

/// Extensions that make a top-level file configuration.
const CIVIC_EXTENSIONS: &[&str] = &[
    "toml",
    "json",
    "yaml",
    "yml",
    "ini",
    "cfg",
    "conf",
    "lock",
    "properties",
    "gradle",
    "mk",
];

// ---------------------------------------------------------------------------
// Monuments
// ---------------------------------------------------------------------------

/// What kind of entry point a file is (PRD §8's first monument criterion).
///
/// Ordered by how much orientation it carries: a `main` is the way into the
/// program, an `index` is the way into a directory. [`Ord`] follows that order,
/// so sorting a list of these puts the strongest anchor first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EntryPointKind {
    /// `main.rs`, `main.go`, `__main__.py`, `Program.cs`. The way in.
    Main,
    /// `lib.rs` — the root of a library crate, one per crate.
    LibraryRoot,
    /// A command-line root: `cli.rs`, `manage.py`, anything in `src/bin/`.
    Cli,
    /// A server or application root: `server.ts`, `wsgi.py`, `app.py`.
    Server,
    /// A route table: `routes.ts`, `router.rs`, Django's `urls.py`. PRD §8 names
    /// these explicitly — a route table is a map of the whole surface.
    Router,
    /// `index.ts`, `App.tsx`. The way into a directory rather than a program.
    Index,
}

impl EntryPointKind {
    /// How strongly this kind anchors the map, in `0.0..=1.0`.
    ///
    /// Feeds [`Monument::score`]. The spread is what decides which of a hundred
    /// `index.ts` files and one `main.rs` gets labelled first when the operator
    /// is zoomed out and only a handful of labels fit.
    pub fn weight(self) -> f64 {
        match self {
            Self::Main => 1.0,
            Self::LibraryRoot => 0.85,
            Self::Cli => 0.8,
            Self::Server => 0.75,
            Self::Router => 0.6,
            Self::Index => 0.5,
        }
    }
}

/// File names that are entry points, and what kind.
///
/// Matched case-insensitively against the whole file name. `mod.rs` and
/// `__init__.py` are deliberately absent: there is one of each per directory, so
/// promoting them would make every directory a monument and nothing an anchor.
const ENTRY_POINTS: &[(&str, EntryPointKind)] = &[
    ("main.rs", EntryPointKind::Main),
    ("main.go", EntryPointKind::Main),
    ("main.py", EntryPointKind::Main),
    ("main.ts", EntryPointKind::Main),
    ("main.js", EntryPointKind::Main),
    ("main.tsx", EntryPointKind::Main),
    ("main.c", EntryPointKind::Main),
    ("main.cc", EntryPointKind::Main),
    ("main.cpp", EntryPointKind::Main),
    ("main.java", EntryPointKind::Main),
    ("main.kt", EntryPointKind::Main),
    ("main.swift", EntryPointKind::Main),
    ("__main__.py", EntryPointKind::Main),
    ("program.cs", EntryPointKind::Main),
    ("lib.rs", EntryPointKind::LibraryRoot),
    ("cli.rs", EntryPointKind::Cli),
    ("cli.ts", EntryPointKind::Cli),
    ("cli.js", EntryPointKind::Cli),
    ("cli.py", EntryPointKind::Cli),
    ("manage.py", EntryPointKind::Cli),
    ("server.ts", EntryPointKind::Server),
    ("server.js", EntryPointKind::Server),
    ("server.py", EntryPointKind::Server),
    ("server.rs", EntryPointKind::Server),
    ("app.py", EntryPointKind::Server),
    // Express's canonical root. `.tsx`/`.jsx` are a component, not a server, and
    // are listed under `Index` below.
    ("app.js", EntryPointKind::Server),
    ("app.ts", EntryPointKind::Server),
    ("wsgi.py", EntryPointKind::Server),
    ("asgi.py", EntryPointKind::Server),
    ("application.py", EntryPointKind::Server),
    ("routes.rs", EntryPointKind::Router),
    ("routes.ts", EntryPointKind::Router),
    ("routes.tsx", EntryPointKind::Router),
    ("routes.js", EntryPointKind::Router),
    ("routes.py", EntryPointKind::Router),
    ("router.rs", EntryPointKind::Router),
    ("router.ts", EntryPointKind::Router),
    ("router.js", EntryPointKind::Router),
    ("urls.py", EntryPointKind::Router),
    ("index.ts", EntryPointKind::Index),
    ("index.tsx", EntryPointKind::Index),
    ("index.js", EntryPointKind::Index),
    ("index.jsx", EntryPointKind::Index),
    ("index.mjs", EntryPointKind::Index),
    ("index.html", EntryPointKind::Index),
    ("app.tsx", EntryPointKind::Index),
    ("app.jsx", EntryPointKind::Index),
    ("app.vue", EntryPointKind::Index),
    ("app.svelte", EntryPointKind::Index),
];

/// Whether a path is one of PRD §8's named entry points.
///
/// Path-only and allocation-light. Two structural rules beyond the name table:
/// a `.rs` file directly inside a `bin/` directory is a Rust binary root, and a
/// `main.go` inside `cmd/<name>/` is already caught by the name.
pub fn entry_point(path: &LogicalPath) -> Option<EntryPointKind> {
    let name = path.file_name()?;
    if let Some((_, kind)) = ENTRY_POINTS
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
    {
        return Some(*kind);
    }
    // `src/bin/polis.rs` — Cargo's other way of declaring a binary.
    let in_bin = path
        .parent()
        .and_then(|p| p.file_name().map(|n| n.eq_ignore_ascii_case("bin")))
        .unwrap_or(false);
    if in_bin
        && path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("rs"))
    {
        return Some(EntryPointKind::Cli);
    }
    None
}

/// The fraction of import-receiving files PRD §8 calls "the top decile".
pub const MONUMENT_INBOUND_DECILE: f64 = 0.10;

/// A file PRD §8 would label at every zoom, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Monument {
    /// The file.
    pub path: LogicalPath,
    /// Which named entry point it is, if it is one.
    pub entry: Option<EntryPointKind>,
    /// How many internal imports point at it.
    pub inbound: u32,
    /// Whether it is in the top decile of inbound imports.
    pub top_decile: bool,
    /// Ranking key, descending. Not a coordinate and not `f32`: it never reaches
    /// geometry, and `f64` keeps the arithmetic exact enough that two files with
    /// genuinely different inputs never tie by rounding.
    pub score: f64,
}

/// Ranks every monument candidate in a tree, strongest anchor first.
///
/// > Landmarks carry the wayfinding, not addresses. Invest here. […] Landmarks
/// > are the mitigation and must not be treated as polish. (PRD §8)
///
/// The ranking is the product, not the set: monuments are "always labelled at
/// every zoom", and at the zoom where the whole city fits on screen there is
/// room for perhaps a dozen labels. Which dozen is this function's answer, so it
/// combines three signals rather than picking one:
///
/// * **What kind of entry point it is** ([`EntryPointKind::weight`]) — a `main`
///   outranks an `index`.
/// * **How much of the repository points at it** — inbound degree, normalised
///   against the most-imported file so the number means the same thing in a
///   50-file repository and a 50 000-file one.
/// * **How shallow it is** — a monument near the root is visible from further
///   away and describes more of the map. Worth a quarter of a point at the root,
///   decaying with depth.
///
/// `inbound` is `(path, count)`, taken as a parameter rather than computed:
/// [`crate::imports::ImportGraph::inbound_counts`] is the natural source, but
/// the ranking must also work before any file has been parsed, and it does —
/// with an empty slice it ranks entry points alone.
///
/// Industrial files are never candidates. Ties break on path, so the result is
/// byte-identical on every machine (PRD §7.4).
pub fn monuments(tree: &RepoTree, inbound: &[(LogicalPath, u32)]) -> Vec<Monument> {
    let mut degree: BTreeMap<&LogicalPath, u32> = BTreeMap::new();
    for (path, count) in inbound {
        // Keyed on the *tree's* copy of the path, so a caller's degree table for
        // a file that has no building contributes nothing and cannot skew the
        // decile.
        if let Some((key, meta)) = tree.files.get_key_value(path) {
            if meta.class != FileClass::Industrial {
                let entry = degree.entry(key).or_insert(0);
                *entry = entry.saturating_add(*count);
            }
        }
    }

    // The top decile, by inbound degree, of the files that receive any import at
    // all. Counting the zero-inbound files would put the ninetieth percentile at
    // zero in any normal repository and promote everything.
    let mut ranked: Vec<(&LogicalPath, u32)> = degree
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(path, count)| (*path, *count))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let decile_size = if ranked.is_empty() {
        0
    } else {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a repository with 2^53 importable files does not exist; the result is clamped"
        )]
        let raw = (ranked.len() as f64 * MONUMENT_INBOUND_DECILE).ceil() as usize;
        // At least one: a repository with nine importable files still has a
        // most-imported file, and PRD §8 needs an anchor in it.
        raw.clamp(1, ranked.len())
    };
    let top: BTreeSet<&LogicalPath> = ranked.iter().take(decile_size).map(|(p, _)| *p).collect();
    let max_inbound = f64::from(ranked.first().map_or(0, |(_, c)| *c));

    let mut out = Vec::new();
    for meta in tree.files.values() {
        if meta.class == FileClass::Industrial {
            continue;
        }
        let entry = entry_point(&meta.path);
        let top_decile = top.contains(&meta.path);
        if entry.is_none() && !top_decile {
            continue;
        }
        let inbound = degree.get(&meta.path).copied().unwrap_or(0);
        let normalised = if max_inbound > 0.0 {
            f64::from(inbound) / max_inbound
        } else {
            0.0
        };
        // Shallower is a better anchor: it is visible from further out and
        // describes more of the map. A quarter of a point at the root, decaying.
        let depth_bonus =
            0.25 / (1.0 + f64::from(u32::try_from(meta.path.depth()).unwrap_or(u32::MAX)));
        out.push(Monument {
            path: meta.path.clone(),
            entry,
            inbound,
            top_decile,
            score: entry.map_or(0.0, EntryPointKind::weight) + normalised + depth_bonus,
        });
    }
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

/// Inbound import degree per file, counted from edges.
///
/// **Distinct importing files, not occurrences.** Four `use crate::auth::…`
/// lines in one module are one file depending on another, and counting them
/// four times would let a single verbose importer manufacture a monument.
/// [`crate::imports::ImportGraph::inbound_counts`] counts the same way, and the
/// two must agree: they feed the same decile threshold, and a file that is a
/// monument by one count and not by the other is a label that flickers.
///
/// Only [`crate::ImportTarget::Internal`] edges count — an external package has
/// no building to be a monument. Self-imports are ignored: a file that mentions
/// itself is not being depended on.
///
/// This exists so [`promote_monuments`] does not have to call into
/// [`crate::imports`] for a number it can derive from the edges it was handed.
pub fn inbound_degree(edges: &[ImportEdge]) -> BTreeMap<LogicalPath, u32> {
    let mut pairs: BTreeSet<(&LogicalPath, &LogicalPath)> = BTreeSet::new();
    for edge in edges {
        if let Some(target) = edge.to.internal() {
            if *target != edge.from {
                pairs.insert((target, &edge.from));
            }
        }
    }
    let mut degree: BTreeMap<LogicalPath, u32> = BTreeMap::new();
    for (target, _) in pairs {
        *degree.entry(target.clone()).or_insert(0) += 1;
    }
    degree
}

/// Applies PRD §8's remaining monument criterion: the top decile of inbound
/// imports.
///
/// Run after [`crate::imports`] has a graph. Separate from [`classify`] so that
/// classification stays a pure function of the path and the layout cannot come
/// to depend on when the import extraction finished.
///
/// Returns how many files this call *changed* — the entry points [`classify`]
/// already found are not counted again.
pub fn promote_monuments(tree: &mut RepoTree, edges: &[ImportEdge]) -> usize {
    let degree = inbound_degree(edges);
    let inbound: Vec<(LogicalPath, u32)> = degree.into_iter().collect();
    promote_monuments_from_inbound(tree, &inbound)
}

/// [`promote_monuments`] from a precomputed inbound degree table.
///
/// The form to use when the degrees came from somewhere other than a slice of
/// edges — an incremental graph update, a cached index, a test fixture.
pub fn promote_monuments_from_inbound(
    tree: &mut RepoTree,
    inbound: &[(LogicalPath, u32)],
) -> usize {
    let promote: Vec<LogicalPath> = monuments(tree, inbound)
        .into_iter()
        .map(|m| m.path)
        .collect();
    let mut changed = 0;
    for path in promote {
        if let Some(meta) = tree.files.get_mut(&path) {
            // Industrial is excluded by `monuments`; a civic square keeps its
            // class, because PRD §8 gives the historic centre a different
            // rendering and a monument in the middle of it is not an
            // improvement.
            if meta.class == FileClass::Ordinary {
                meta.class = FileClass::Monument;
                changed += 1;
            }
        }
    }
    changed
}

// ---------------------------------------------------------------------------
// Decay
// ---------------------------------------------------------------------------

/// True once a file has gone 90 days untouched (PRD §8, §7.5).
///
/// Rendered desaturated with a softened outline and encroaching vegetation.
/// Together with vacant lots this makes dead code visible without anyone running
/// an analysis.
///
/// `now` is passed in rather than read: nothing in this crate may touch the wall
/// clock, because overgrowth is a rendering input and a layout that changed with
/// the date would break PRD §7.4.
pub fn is_overgrown(last_touched: WallTime, now: WallTime) -> bool {
    last_touched.days_until(now) >= OVERGROWTH_DAYS
}

/// [`is_overgrown`] for a file whose history may be unknown.
///
/// An untracked file's `last_touched` is [`WallTime::UNIX_EPOCH`], which is
/// fifty-odd years ago and would make every brand-new working-tree file render
/// as dead code — the exact opposite of the truth. A file git has never seen is
/// never overgrown.
pub fn is_overgrown_meta(meta: &FileMeta, now: WallTime) -> bool {
    meta.is_tracked() && is_overgrown(meta.last_touched, now)
}

/// PRD §8's overgrowth threshold, in days.
pub const OVERGROWTH_DAYS: u32 = 90;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ImportTarget, RepoTree};

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    fn meta(path: &str, size: u64) -> FileMeta {
        let path = lp(path);
        let mut m = FileMeta::untracked(path.clone(), size);
        m.class = classify(&path);
        m.language = language_for(&path);
        m
    }

    fn tree_of(files: &[(&str, u64)]) -> RepoTree {
        let mut tree = RepoTree::default();
        for (path, size) in files {
            let m = meta(path, *size);
            tree.files.insert(m.path.clone(), m);
        }
        tree
    }

    // -- classification ----------------------------------------------------

    #[test]
    fn industrial_beats_every_other_classification() {
        // The whole point of the precedence: node_modules is full of files that
        // would otherwise be monuments and civic squares.
        assert_eq!(
            classify(&lp("node_modules/react/index.js")),
            FileClass::Industrial
        );
        assert_eq!(
            classify(&lp("node_modules/react/package.json")),
            FileClass::Industrial
        );
        assert_eq!(
            classify(&lp("target/debug/build/x/main.rs")),
            FileClass::Industrial
        );
        assert_eq!(classify(&lp("web/dist/app.js")), FileClass::Industrial);
        assert_eq!(
            classify(&lp("vendor/github.com/x/y.go")),
            FileClass::Industrial
        );
        assert_eq!(
            classify(&lp("src/api/__pycache__/x.pyc")),
            FileClass::Industrial
        );
        // Generated files mixed in with hand-written ones.
        assert_eq!(classify(&lp("src/proto/user.pb.go")), FileClass::Industrial);
        assert_eq!(classify(&lp("src/vendor.min.js")), FileClass::Industrial);
        // ...and the near misses that must stay ordinary.
        assert_eq!(
            classify(&lp("src/distributed/queue.rs")),
            FileClass::Ordinary
        );
        assert_eq!(classify(&lp("src/targeting/aim.rs")), FileClass::Ordinary);
        assert_eq!(classify(&lp("src/builder.rs")), FileClass::Ordinary);
        // A file whose *own* name is a build directory name is still a file.
        assert_eq!(classify(&lp("build")), FileClass::Ordinary);
    }

    #[test]
    fn monuments_and_civic_squares_are_classified_by_path_alone() {
        assert_eq!(classify(&lp("src/main.rs")), FileClass::Monument);
        assert_eq!(classify(&lp("src/lib.rs")), FileClass::Monument);
        assert_eq!(classify(&lp("src/bin/polis.rs")), FileClass::Monument);
        assert_eq!(classify(&lp("web/src/routes.ts")), FileClass::Monument);
        assert_eq!(classify(&lp("api/urls.py")), FileClass::Monument);
        assert_eq!(
            classify(&lp("web/components/index.ts")),
            FileClass::Monument
        );
        // A top-level entry point is a monument first, config second.
        assert_eq!(classify(&lp("index.js")), FileClass::Monument);

        assert_eq!(classify(&LogicalPath::root()), FileClass::CivicSquare);
        assert_eq!(classify(&lp("Cargo.toml")), FileClass::CivicSquare);
        assert_eq!(classify(&lp("package.json")), FileClass::CivicSquare);
        assert_eq!(classify(&lp("README.md")), FileClass::CivicSquare);
        assert_eq!(classify(&lp("LICENSE")), FileClass::CivicSquare);
        assert_eq!(classify(&lp("Makefile")), FileClass::CivicSquare);
        assert_eq!(classify(&lp(".gitignore")), FileClass::CivicSquare);
        // Depth is the whole difference: a nested config is work, not orientation.
        assert_eq!(classify(&lp("src/auth/config.toml")), FileClass::Ordinary);
        assert_eq!(classify(&lp("docs/README.md")), FileClass::Ordinary);
        // mod.rs and __init__.py are everywhere; they anchor nothing.
        assert_eq!(classify(&lp("src/auth/mod.rs")), FileClass::Ordinary);
        assert_eq!(classify(&lp("api/__init__.py")), FileClass::Ordinary);
    }

    #[test]
    fn entry_point_kinds_rank_the_way_the_labels_should() {
        assert_eq!(entry_point(&lp("src/main.rs")), Some(EntryPointKind::Main));
        assert_eq!(
            entry_point(&lp("SRC/MAIN.RS")),
            Some(EntryPointKind::Main),
            "ADR-0028 folds ASCII case everywhere"
        );
        assert_eq!(
            entry_point(&lp("src/lib.rs")),
            Some(EntryPointKind::LibraryRoot)
        );
        assert_eq!(
            entry_point(&lp("src/bin/tool.rs")),
            Some(EntryPointKind::Cli)
        );
        assert_eq!(
            entry_point(&lp("app/bin/tool.py")),
            None,
            "bin/ is a Rust rule"
        );
        assert_eq!(
            entry_point(&lp("api/wsgi.py")),
            Some(EntryPointKind::Server)
        );
        assert_eq!(
            entry_point(&lp("web/router.ts")),
            Some(EntryPointKind::Router)
        );
        assert_eq!(
            entry_point(&lp("web/index.tsx")),
            Some(EntryPointKind::Index)
        );
        assert_eq!(entry_point(&lp("src/auth.rs")), None);

        let mut kinds = [
            EntryPointKind::Index,
            EntryPointKind::Main,
            EntryPointKind::Router,
            EntryPointKind::LibraryRoot,
        ];
        kinds.sort_unstable();
        assert_eq!(
            kinds[0],
            EntryPointKind::Main,
            "the strongest anchor sorts first"
        );
        assert!(EntryPointKind::Main.weight() > EntryPointKind::Index.weight());
    }

    #[test]
    fn languages_come_from_extensions_and_tsx_is_its_own_grammar() {
        assert_eq!(language_for(&lp("src/main.rs")), Some(Language::Rust));
        assert_eq!(language_for(&lp("web/a.mjs")), Some(Language::JavaScript));
        assert_eq!(language_for(&lp("web/a.cjs")), Some(Language::JavaScript));
        assert_eq!(language_for(&lp("web/a.ts")), Some(Language::TypeScript));
        assert_eq!(language_for(&lp("web/a.d.ts")), Some(Language::TypeScript));
        assert_eq!(language_for(&lp("web/a.tsx")), Some(Language::Tsx));
        assert_eq!(language_for(&lp("api/a.pyi")), Some(Language::Python));
        assert_eq!(language_for(&lp("SRC/A.RS")), Some(Language::Rust));
        // No grammar is a normal condition, not a failure.
        assert_eq!(language_for(&lp("README.md")), None);
        assert_eq!(language_for(&lp("Makefile")), None);
        assert_eq!(language_for(&lp(".gitignore")), None);
    }

    // -- configurable industrial rules -------------------------------------

    #[test]
    fn the_industrial_rule_is_configurable_in_both_directions() {
        let mut rules = IndustrialRules::default();
        assert!(rules.is_industrial_dir(&lp("node_modules")));
        assert!(rules.is_industrial_dir(&lp("web/node_modules/react")));
        assert!(is_massed_tree(&lp("target/debug")));
        assert!(!is_massed_tree(&lp("src/auth")));

        // A repository whose dist/ is real, hand-written code.
        assert!(rules.remove_dir("dist"));
        assert!(!rules.is_industrial_dir(&lp("web/dist")));
        assert_eq!(
            classify_with(&lp("web/dist/index.js"), &rules),
            FileClass::Monument,
            "with dist/ no longer industrial, its entry point anchors again"
        );
        assert_eq!(
            classify_with(&lp("web/dist/style.css"), &rules),
            FileClass::Ordinary
        );

        // ...and an ecosystem the shipped list has never heard of.
        rules.push_dir("_generated_stubs");
        rules.push_prefix("docs/api/reference");
        rules.push_generated_suffix(".gen.kt");
        assert!(rules.is_industrial_file(&lp("src/_generated_stubs/x.rs")));
        assert!(rules.is_industrial_file(&lp("docs/api/reference/index.md")));
        assert!(rules.is_industrial_file(&lp("src/models/User.gen.kt")));
        assert!(!rules.is_industrial_file(&lp("docs/api/guide.md")));

        // The shipped rules are untouched by any of that.
        assert!(is_massed_tree(&lp("web/dist")));
        assert!(!IndustrialRules::empty().is_industrial_dir(&lp("node_modules")));
        assert!(IndustrialRules::empty().is_empty());
    }

    // -- the district tree -------------------------------------------------

    #[test]
    fn district_aggregates_count_every_descendant_once() {
        let tree = tree_of(&[
            ("README.md", 100),
            ("src/main.rs", 200),
            ("src/auth/mod.rs", 40),
            ("src/auth/session/token.rs", 8),
            ("web/index.ts", 1),
        ]);
        let districts = DistrictTree::from_repo(&tree, &IndustrialRules::default());

        let root = districts.root();
        assert_eq!(root.kind, DistrictKind::CivicSquare);
        assert_eq!(root.file_count, 5);
        assert_eq!(root.direct_file_count, 1, "only README.md is at the root");
        assert_eq!(root.total_size_bytes, 349);
        assert_eq!(root.max_depth, 4, "src/auth/session/token.rs");
        assert_eq!(
            root.children
                .iter()
                .map(LogicalPath::as_str)
                .collect::<Vec<_>>(),
            ["src", "web"]
        );

        let src = districts.district(&lp("src")).expect("src");
        assert_eq!(src.file_count, 3);
        assert_eq!(src.direct_file_count, 1);
        assert_eq!(src.total_size_bytes, 248);
        assert_eq!(src.max_depth, 3, "auth/session/token.rs is three below src");

        let session = districts
            .district(&lp("src/auth/session"))
            .expect("session");
        assert_eq!(session.file_count, 1);
        assert_eq!(session.max_depth, 1);
        assert!(session.children.is_empty());

        // Every intermediate directory exists even though nothing named it.
        assert!(districts.district(&lp("src/auth")).is_some());
        assert_eq!(
            districts.len(),
            5,
            "root, src, src/auth, src/auth/session, web"
        );

        // Deterministic ordering, every launch, every filesystem.
        let order: Vec<&str> = districts.districts().map(|d| d.path.as_str()).collect();
        assert_eq!(order, ["", "src", "src/auth", "src/auth/session", "web"]);
        let kids: Vec<&str> = districts
            .children(&lp("src"))
            .map(|d| d.path.as_str())
            .collect();
        assert_eq!(kids, ["src/auth"]);
    }

    #[test]
    fn an_empty_repository_still_has_a_civic_square() {
        let districts = DistrictTree::from_repo(&RepoTree::default(), &IndustrialRules::default());
        assert_eq!(districts.len(), 1);
        assert!(districts.is_empty());
        assert_eq!(districts.root().file_count, 0);
        assert_eq!(districts.root().max_depth, 0);
        assert_eq!(districts.root().kind, DistrictKind::CivicSquare);
    }

    #[test]
    fn industrial_districts_are_classified_all_the_way_down() {
        let tree = tree_of(&[
            ("src/main.rs", 1),
            ("node_modules/react/index.js", 10),
            ("node_modules/react/lib/deep.js", 20),
        ]);
        let districts = DistrictTree::from_repo(&tree, &IndustrialRules::default());
        for path in [
            "node_modules",
            "node_modules/react",
            "node_modules/react/lib",
        ] {
            let d = districts.district(&lp(path)).expect(path);
            assert_eq!(d.kind, DistrictKind::Industrial, "{path}");
            assert!(d.is_massed());
        }
        assert_eq!(
            districts.district(&lp("src")).expect("src").kind,
            DistrictKind::Normal
        );
        // The mass still has a size: PRD §8 draws it, it does not omit it.
        assert_eq!(
            districts
                .district(&lp("node_modules"))
                .expect("nm")
                .total_size_bytes,
            30
        );
    }

    #[test]
    fn two_worktrees_of_one_repo_build_one_district_tree() {
        // PRD §7.6: `/repo-wt-3/src/auth.ts` and `/repo/src/auth.ts` are the same
        // logical file in two physical places. Getting this wrong "means seven
        // near-identical maps side by side".
        let base = std::env::temp_dir().join("polis-tree-test");
        let primary = base.join("repo");
        let worktree = base.join("repo-wt-3");

        let mut mapper = polis_events::PathMapper::new(&primary).expect("mapper");
        mapper
            .add_worktree(WorktreeId(3), &worktree)
            .expect("worktree");

        let physical = [
            (primary.join("src").join("auth.ts"), 10_u64),
            (primary.join("src").join("main.ts"), 20),
            (worktree.join("src").join("auth.ts"), 11),
            (worktree.join("tests").join("auth.test.ts"), 5),
        ];
        let mut files: BTreeMap<LogicalPath, FileMeta> = BTreeMap::new();
        let mut seen_worktrees = BTreeSet::new();
        for (path, size) in &physical {
            let (id, logical) = mapper.to_logical(path).expect("inside a known root");
            seen_worktrees.insert(id);
            // The layout key is the logical path, so the second worktree's copy
            // lands on the building the first one already has.
            files
                .entry(logical.clone())
                .or_insert_with(|| meta(logical.as_str(), *size));
        }

        assert_eq!(seen_worktrees.len(), 2, "both checkouts were recognised");
        let districts = DistrictTree::from_files(files.values(), &IndustrialRules::default());
        assert_eq!(
            districts
                .districts()
                .map(|d| d.path.as_str())
                .collect::<Vec<_>>(),
            ["", "src", "tests"],
            "one city, not one per checkout"
        );
        assert_eq!(districts.root().file_count, 3, "auth.ts is one building");
        assert_eq!(districts.district(&lp("src")).expect("src").file_count, 2);
    }

    // -- monuments ---------------------------------------------------------

    #[test]
    fn monument_ranking_puts_the_strongest_anchor_first() {
        let tree = tree_of(&[
            ("src/main.rs", 1),
            ("src/auth/session.rs", 1),
            ("src/auth/token.rs", 1),
            ("src/util/log.rs", 1),
            ("src/util/fmt.rs", 1),
            ("web/components/index.ts", 1),
            ("web/components/button.tsx", 1),
            ("docs/design.md", 1),
            ("node_modules/react/index.js", 1),
        ]);
        // A synthetic graph with known degrees: session.rs is the hub.
        let inbound = [
            (lp("src/auth/session.rs"), 40),
            (lp("src/util/log.rs"), 12),
            (lp("src/auth/token.rs"), 3),
            (lp("web/components/button.tsx"), 1),
            (lp("node_modules/react/index.js"), 999),
        ];

        let ranked = monuments(&tree, &inbound);
        let names: Vec<&str> = ranked.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(
            names,
            [
                // entry point (1.0) + max inbound share (0.0) + depth 2 (0.083)
                "src/main.rs",
                // hub: no entry point, full inbound share (1.0), depth 3 (0.0625)
                "src/auth/session.rs",
                // entry point (0.5) + depth 3 (0.0625)
                "web/components/index.ts",
            ],
            "{ranked:#?}"
        );
        assert_eq!(ranked[1].inbound, 40);
        assert!(ranked[1].top_decile);
        assert!(ranked[0].entry.is_some() && !ranked[0].top_decile);
        assert!(
            !names.contains(&"node_modules/react/index.js"),
            "an industrial file is never a monument however imported it is"
        );
        assert!(
            !names.contains(&"src/util/log.rs"),
            "second place by degree is still outside a five-file top decile"
        );

        // Ranking with no import data at all still works, and ranks by kind.
        let no_imports = monuments(&tree, &[]);
        assert_eq!(no_imports[0].path.as_str(), "src/main.rs");
        assert!(no_imports.iter().all(|m| !m.top_decile));
    }

    #[test]
    fn the_top_decile_is_a_decile_of_the_files_that_receive_imports() {
        // Twenty importable files: the decile is two, not "everything with an
        // inbound edge" and not a fraction of the whole repository.
        let paths: Vec<String> = (0..20).map(|i| format!("src/f{i:02}.rs")).collect();
        let files: Vec<(&str, u64)> = paths.iter().map(|p| (p.as_str(), 1)).collect();
        let tree = tree_of(&files);
        let inbound: Vec<(LogicalPath, u32)> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| (lp(p), u32::try_from(i).expect("small") + 1))
            .collect();

        let ranked = monuments(&tree, &inbound);
        assert_eq!(ranked.len(), 2, "{ranked:#?}");
        assert_eq!(ranked[0].path.as_str(), "src/f19.rs");
        assert_eq!(ranked[1].path.as_str(), "src/f18.rs");
        assert!(ranked.iter().all(|m| m.top_decile));

        // One importable file: the decile is that one file, never zero.
        let tree = tree_of(&[("src/a.rs", 1), ("src/b.rs", 1)]);
        let ranked = monuments(&tree, &[(lp("src/a.rs"), 1)]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].path.as_str(), "src/a.rs");
    }

    #[test]
    fn promotion_counts_only_what_it_changed() {
        let mut tree = tree_of(&[
            ("src/main.rs", 1),
            ("src/auth/session.rs", 1),
            ("src/auth/token.rs", 1),
            ("Cargo.toml", 1),
        ]);
        let edges = [
            edge("src/main.rs", "src/auth/session.rs"),
            edge("src/auth/token.rs", "src/auth/session.rs"),
            // A second `use` from a file that already imports it is the same
            // relationship, exactly as `ImportGraph::inbound_counts` sees it.
            edge("src/main.rs", "src/auth/session.rs"),
            // Self-imports and external targets contribute nothing.
            edge("src/auth/session.rs", "src/auth/session.rs"),
            external("src/main.rs"),
        ];
        let degree = inbound_degree(&edges);
        assert_eq!(
            degree.get(&lp("src/auth/session.rs")),
            Some(&2),
            "two importing files, three edges"
        );
        assert_eq!(degree.len(), 1, "self and external edges are not degree");

        let changed = promote_monuments(&mut tree, &edges);
        assert_eq!(changed, 1, "main.rs was already a monument by name");
        assert_eq!(
            tree.files[&lp("src/auth/session.rs")].class,
            FileClass::Monument
        );
        assert_eq!(tree.files[&lp("src/main.rs")].class, FileClass::Monument);
        assert_eq!(tree.files[&lp("Cargo.toml")].class, FileClass::CivicSquare);
        // Idempotent: running it again changes nothing.
        assert_eq!(promote_monuments(&mut tree, &edges), 0);
    }

    fn edge(from: &str, to: &str) -> ImportEdge {
        ImportEdge {
            from: lp(from),
            to: ImportTarget::Internal(lp(to)),
            specifier: to.to_owned(),
            language: Language::Rust,
        }
    }

    fn external(from: &str) -> ImportEdge {
        ImportEdge {
            from: lp(from),
            to: ImportTarget::External,
            specifier: "serde".to_owned(),
            language: Language::Rust,
        }
    }

    // -- the walk ----------------------------------------------------------

    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, bytes).expect("write");
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "README.md", b"# repo");
        write(root, "Cargo.toml", b"[package]");
        write(root, "src/main.rs", b"fn main() {}");
        write(root, "src/auth/session.rs", b"pub struct Session;");
        write(root, "src/auth/token.rs", b"pub struct Token;");
        write(root, "node_modules/react/index.js", b"module.exports = {};");
        write(root, "node_modules/react/lib/deep.js", b"//");
        write(root, ".git/objects/ab/cdef", b"not a source file");
        write(root, ".git/HEAD", b"ref: refs/heads/main");
        dir
    }

    // -----------------------------------------------------------------------
    // The repo-walk trap
    // -----------------------------------------------------------------------

    /// **The regression test for the repo-walk trap.**
    ///
    /// Walk, drop a render into an excluded directory the way a Polis run does,
    /// walk again: the two results must be byte-identical. Written as a
    /// *sequence* rather than as two independent walks on purpose — the bug was
    /// a feedback loop between consecutive runs, and only a test that runs them
    /// in order can catch it.
    ///
    /// Four writes, because the trap has four shapes: a new file in the excluded
    /// subtree, a new *subdirectory* of it, a file in Polis's own state
    /// directory, and an overwrite of a file that was already there.
    #[test]
    fn a_file_written_into_an_excluded_directory_cannot_move_the_city() {
        let dir = fixture();
        let root = dir.path();
        write(root, "docs/design/DESIGN.md", b"# the winning architecture");
        write(root, "docs/guide.md", b"# a real document");

        let before = walk(root).expect("walk");
        assert!(
            before.iter().any(|f| f.path.as_str() == "docs/guide.md"),
            "the exclusion must not take the whole of docs/ with it"
        );
        assert!(
            !before
                .iter()
                .any(|f| f.path.as_str().starts_with("docs/design")),
            "docs/design is excluded by default"
        );

        // Exactly what a run does between two other runs.
        write(
            root,
            "docs/design/city-m1.png",
            b"\x89PNG\r\n\x1a\n0123456789",
        );
        write(
            root,
            "docs/design/renders/large.png",
            b"\x89PNG\r\n\x1a\nmore",
        );
        write(root, ".polis/state.json", b"{}");
        write(
            root,
            "docs/design/DESIGN.md",
            b"# rewritten, and much longer",
        );

        let after = walk(root).expect("walk");
        assert_eq!(
            before.len(),
            after.len(),
            "the walk grew by {} files that Polis itself wrote",
            after.len().saturating_sub(before.len())
        );
        for (a, b) in before.iter().zip(after.iter()) {
            assert_eq!(a.path, b.path, "the walk reordered");
            assert_eq!(a.size_bytes, b.size_bytes, "{} changed size", a.path);
            assert_eq!(a.class, b.class, "{} changed class", a.path);
        }
    }

    /// The same corpus with the exclusions turned off: proof the test above is
    /// testing the exclusion and not something else.
    ///
    /// Without this the first test would still pass if `walk` had simply stopped
    /// seeing new files, which is a much worse bug.
    #[test]
    fn without_the_exclusion_the_same_writes_do_move_the_city() {
        let dir = fixture();
        let root = dir.path();
        write(root, "docs/design/DESIGN.md", b"# the winning architecture");
        let faithful = WalkOptions {
            exclusions: Some(WalkExclusions::empty()),
            ..WalkOptions::default()
        };
        let before = walk_with(root, &faithful).expect("walk");
        write(
            root,
            "docs/design/city-m1.png",
            b"\x89PNG\r\n\x1a\n0123456789",
        );
        let after = walk_with(root, &faithful).expect("walk");
        assert_eq!(
            after.len(),
            before.len() + 1,
            "the faithful walk must see the render Polis just wrote"
        );
    }

    /// `--out docs/city.png` must cost one building, not the `docs/` district.
    #[test]
    fn excluding_an_output_file_keeps_its_directory() {
        let dir = fixture();
        let root = dir.path();
        write(root, "docs/guide.md", b"# a real document");
        write(root, "docs/city.png", b"\x89PNG\r\n\x1a\n");

        let mut rules = WalkExclusions::shipped();
        assert!(
            rules.exclude_output(&root.join("docs").join("city.png"), root),
            "an output inside the repository is excluded"
        );
        assert!(
            !rules.exclude_output(Path::new("elsewhere.png"), root),
            "an output outside the repository cannot be walked, so it is not excluded"
        );

        let files = walk_with(
            root,
            &WalkOptions {
                exclusions: Some(rules),
                ..WalkOptions::default()
            },
        )
        .expect("walk");
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"docs/guide.md"), "{paths:?}");
        assert!(!paths.contains(&"docs/city.png"), "{paths:?}");
    }

    /// The exclusions are rules, not a hardcoded list: both directions.
    #[test]
    fn the_exclusions_are_configurable_in_both_directions() {
        let mut rules = WalkExclusions::shipped();
        assert!(rules.excludes_dir(&lp("docs/design")));
        assert!(rules.excludes_file(&lp("docs/design/deep/city.png")));
        assert!(
            rules.excludes_dir(&lp("web/.polis")),
            "matched at any depth"
        );
        assert!(!rules.excludes_file(&lp("docs/guide.md")));
        // A file *called* .polis at the root is configuration, not state.
        assert!(!rules.excludes_file(&lp(".polis")));

        // "our docs/design is hand-written prose."
        let mut opened = WalkExclusions::empty();
        opened.push_dir(".polis");
        assert!(!opened.excludes_file(&lp("docs/design/DESIGN.md")));

        // "our generated docs live in build/api."
        rules.push_path("build/api");
        assert!(rules.excludes_file(&lp("build/api/index.html")));
        assert!(!rules.excludes_file(&lp("build/main.rs")));

        assert!(rules.remove_dir(".polis"));
        assert!(!rules.excludes_dir(&lp("web/.polis")));
        assert!(!rules.is_empty());
        assert!(WalkExclusions::empty().is_empty());
    }

    /// Matching folds ASCII case, like [`LogicalPath`] itself (ADR-0028).
    #[test]
    fn exclusion_matching_folds_ascii_case() {
        let rules = WalkExclusions::shipped();
        assert!(rules.excludes_file(&lp("DOCS/DESIGN/city.png")));
        assert!(rules.excludes_dir(&lp("web/.POLIS")));
    }

    #[test]
    fn the_walk_is_sorted_sized_classified_and_skips_dot_git() {
        let dir = fixture();
        let files = walk(dir.path()).expect("walk");
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Cargo.toml",
                // Ahead of README.md because `LogicalPath`'s `Ord` folds ASCII
                // case (ADR-0028): the comparison is `node_modules` < `readme`.
                "node_modules/react/index.js",
                "node_modules/react/lib/deep.js",
                "README.md",
                "src/auth/session.rs",
                "src/auth/token.rs",
                "src/main.rs",
            ],
            "sorted by logical path, and .git is never a district"
        );
        let main = files
            .iter()
            .find(|f| f.path.as_str() == "src/main.rs")
            .expect("main");
        assert_eq!(main.size_bytes, 12);
        assert_eq!(main.class, FileClass::Monument);
        assert_eq!(main.language, Some(Language::Rust));
        assert!(!main.is_tracked(), "the walk knows nothing about git");
        let react = files
            .iter()
            .find(|f| f.path.as_str().starts_with("node_"))
            .expect("nm");
        assert_eq!(react.class, FileClass::Industrial);

        // The same walk, run again, is byte-identical (PRD §7.4).
        assert_eq!(files, walk(dir.path()).expect("walk again"));
    }

    #[test]
    fn skip_massed_drops_the_dependency_tree_and_nothing_else() {
        let dir = fixture();
        let opts = WalkOptions {
            skip_massed: true,
            ..WalkOptions::default()
        };
        let files = walk_with(dir.path(), &opts).expect("walk");
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Cargo.toml",
                "README.md",
                "src/auth/session.rs",
                "src/auth/token.rs",
                "src/main.rs"
            ]
        );

        // max_files is a safety valve that returns a smaller city, not an error.
        let opts = WalkOptions {
            max_files: Some(2),
            ..WalkOptions::default()
        };
        assert_eq!(walk_with(dir.path(), &opts).expect("walk").len(), 2);
    }

    #[test]
    fn walking_a_missing_root_is_an_error_but_a_vanished_subdirectory_is_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(walk(&dir.path().join("nope")).is_err());
        assert!(walk(dir.path()).expect("empty walk").is_empty());
    }

    // -- the index ---------------------------------------------------------

    #[test]
    fn the_index_builds_a_tree_and_refreshes_incrementally() {
        let dir = fixture();
        let mut index = RepoIndex::open(dir.path()).expect("open");
        assert_eq!(index.tree().files.len(), 7);
        assert_eq!(
            index
                .tree()
                .worktrees
                .get(&WorktreeId::PRIMARY)
                .map(PathBuf::as_path),
            Some(dir.path())
        );
        assert!(index.tree().head.is_empty(), "HEAD is git's to supply");
        assert_eq!(
            index
                .districts()
                .district(&lp("src"))
                .expect("src")
                .file_count,
            3
        );

        // Nothing changed.
        assert!(index.refresh().expect("refresh").is_empty());

        // A new file, a resized file, a deleted file.
        write(dir.path(), "src/auth/refresh.rs", b"pub fn refresh() {}");
        write(dir.path(), "src/main.rs", b"fn main() { println!(); }");
        std::fs::remove_file(dir.path().join("README.md")).expect("rm");

        let delta = index.refresh().expect("refresh");
        assert_eq!(
            delta
                .added
                .iter()
                .map(LogicalPath::as_str)
                .collect::<Vec<_>>(),
            ["src/auth/refresh.rs"]
        );
        assert_eq!(
            delta
                .resized
                .iter()
                .map(LogicalPath::as_str)
                .collect::<Vec<_>>(),
            ["src/main.rs"]
        );
        assert_eq!(
            delta
                .removed
                .iter()
                .map(LogicalPath::as_str)
                .collect::<Vec<_>>(),
            ["README.md"]
        );
        assert!(delta.retouched.is_empty(), "last_touched is a commit time");
        assert_eq!(delta.len(), 3);
        assert_eq!(index.tree().files.len(), 7);
    }

    #[test]
    fn history_survives_a_refresh_and_worktree_ids_are_registration_ordered() {
        let dir = fixture();
        let mut index = RepoIndex::open(dir.path()).expect("open");

        let day = 86_400_000_i64;
        index.apply_growth(&[
            (lp("src/main.rs"), WallTime::from_unix_millis(day)),
            (
                lp("src/auth/session.rs"),
                WallTime::from_unix_millis(2 * day),
            ),
            (
                lp("deleted/long/ago.rs"),
                WallTime::from_unix_millis(3 * day),
            ),
        ]);
        index.apply_last_touched(&[(lp("src/main.rs"), WallTime::from_unix_millis(400 * day))]);
        index.set_head("a460b0e");

        let main = index.tree().file(&lp("src/main.rs")).expect("main").clone();
        assert_eq!(main.growth_index, 0);
        assert!(main.is_tracked());
        assert_eq!(main.added_at, WallTime::from_unix_millis(day));
        assert_eq!(main.last_touched, WallTime::from_unix_millis(400 * day));
        // A file only git knows about gets no building.
        assert!(index.tree().file(&lp("deleted/long/ago.rs")).is_none());

        // A refresh must not throw the history away.
        write(dir.path(), "src/main.rs", b"fn main() { /* edited */ }");
        index.refresh().expect("refresh");
        let after = index.tree().file(&lp("src/main.rs")).expect("main");
        assert_eq!(after.growth_index, 0);
        assert_eq!(after.added_at, main.added_at);
        assert_eq!(after.last_touched, main.last_touched);
        assert_ne!(after.size_bytes, main.size_bytes);
        assert_eq!(index.tree().head, "a460b0e");

        // Worktree ids are handed out in registration order, and are idempotent.
        let wt = dir.path().join("..").join("repo-wt-3");
        let first = index.add_worktree(&wt).expect("register");
        assert_eq!(first, WorktreeId(1));
        assert_eq!(index.add_worktree(&wt).expect("again"), first);
        assert_eq!(
            index.add_worktree(Path::new("/other")).expect("second"),
            WorktreeId(2)
        );
        assert!(!first.is_primary());
    }

    #[test]
    fn a_monument_promotion_survives_a_refresh() {
        let dir = fixture();
        let mut index = RepoIndex::open(dir.path()).expect("open");
        let mut tree = index.tree().clone();
        let promoted = promote_monuments(&mut tree, &[edge("src/main.rs", "src/auth/session.rs")]);
        assert_eq!(promoted, 1);
        // Feed the promotion back in the way an integrator would.
        index.tree = tree;
        write(
            dir.path(),
            "src/auth/session.rs",
            b"pub struct Session { id: u32 }",
        );
        index.refresh().expect("refresh");
        assert_eq!(
            index
                .tree()
                .file(&lp("src/auth/session.rs"))
                .expect("session")
                .class,
            FileClass::Monument,
            "a path-only reclassification must not undo the import graph's answer"
        );
    }

    // -- decay -------------------------------------------------------------

    #[test]
    fn overgrowth_is_ninety_days_and_an_untracked_file_is_never_overgrown() {
        let day = 86_400_000_i64;
        let now = WallTime::from_unix_millis(1_000 * day);
        assert!(is_overgrown(WallTime::from_unix_millis(910 * day), now));
        assert!(!is_overgrown(WallTime::from_unix_millis(911 * day), now));
        assert_eq!(OVERGROWTH_DAYS, 90);

        let mut tracked = FileMeta::untracked(lp("src/old.rs"), 1);
        tracked.growth_index = 3;
        tracked.last_touched = WallTime::from_unix_millis(500 * day);
        assert!(is_overgrown_meta(&tracked, now));

        // A brand-new working-tree file has last_touched = UNIX_EPOCH, which is
        // fifty years ago and is not evidence of anything.
        let fresh = FileMeta::untracked(lp("src/new.rs"), 1);
        assert!(
            is_overgrown(fresh.last_touched, now),
            "the bare test says yes"
        );
        assert!(!is_overgrown_meta(&fresh, now), "and it is wrong to say so");
    }
}
