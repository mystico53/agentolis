//! Neighborhoods: districts at a size a person can read, with a kind and a
//! description (PRD §3, §8, §9, §12).
//!
//! # The problem
//!
//! PRD §3 says a district *is* a directory, and [`crate::tree::DistrictTree`]
//! takes that literally: every directory that holds a file is a district. The
//! renderer then has to pick a level to label and colour, and the level it
//! picked was the top one — so a repository whose code all lives under `src/`
//! gets one enormous district and a scatter of tiny ones, and the operator's own
//! word for it was that the colours "carry no meaning".
//!
//! Neither end of that is wrong on its own. Every directory is too many things
//! to label; the top level is too few. **The level is the thing that should not
//! be fixed.**
//!
//! # The rule
//!
//! Descend into a directory while it holds too large a share of the repository,
//! and stop when a child would be too small to label. Both bounds are derived
//! from the repository's own size ([`NeighborhoodOptions::bounds`]), not
//! hardcoded as a district count — a count would either shred a small repository
//! or fuse a large one.
//!
//! This changes **granularity, not the principle**. PRD §9 is untouched:
//!
//! > **Do not let the import graph fight the directory tree for position.** The
//! > tree determines placement.
//!
//! Every neighborhood is still a directory, still addressed by its path, still
//! placed by the tree. There are simply more of them, at a level chosen per
//! branch instead of per repository.
//!
//! # How each awkward repository degrades
//!
//! | Shape | Result |
//! |---|---|
//! | Everything in the root | One neighborhood, `/`. There is no level to descend to, and inventing one would be a lie about the tree. |
//! | One deep chain, `a/b/c/…` | One neighborhood, named for the leaf. A chain of single-child directories is collapsed on sight ([`Neighborhoods::build`]), because `a`, `a/b` and `a/b/c` are three names for one place. |
//! | A monorepo of ten packages | Ten or more: each package is under the ceiling, so nothing splits, and a package that is not gets split internally. |
//! | Dominated by one vendored folder | The vendored tree is **one** neighborhood and is held out of the sizing budget entirely. Otherwise a 90 000-file `node_modules` sets the scale and the operator's own 1 500 files become a single district — which is exactly backwards. |
//!
//! # Determinism
//!
//! The partition is a pure function of `(paths, sizes, options)`. Every decision
//! is local and compares against thresholds fixed before the walk begins, so the
//! order directories are visited in cannot change the answer — asserted by
//! `the_partition_does_not_depend_on_input_order`. Every collection is a
//! `BTreeMap` or `BTreeSet`, and the display names are resolved by a fixed
//! widening rule rather than by first-come.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use polis_events::LogicalPath;
use serde::{Deserialize, Serialize};

use crate::describe::{DescribeStats, Describer, Description, DescriptionSource};
use crate::kinds::{kind_of_with, CodeKind, KindMix, KindRules, KindRulesConfig};
use crate::llm::Freshness;
use crate::{FileMeta, RepoTree};

// ---------------------------------------------------------------------------
// The public shape
// ---------------------------------------------------------------------------

/// One labelled region of the city: a directory, its files, what kind of code
/// they are, and what it does.
///
/// This is the record a rendering round draws. Everything on it is either a
/// direct fact about the repository or a quotation from it; nothing here is
/// inferred from agent activity, so it is stable between launches in the way
/// PRD §7.4 requires the map to be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Neighborhood {
    /// The directory. The layout key, and the addressing system PRD §9 says the
    /// tree already owns.
    pub path: LogicalPath,
    /// What to write on the map: the shortest suffix of [`Neighborhood::path`]
    /// that no other neighborhood shares. `"/"` for the repository root.
    pub name: String,
    /// The dominant [`CodeKind`] of its own files — the hue.
    ///
    /// Always [`CodeKind::Vendored`] for an industrial tree, whatever its
    /// contents happen to be, because PRD §8 renders that as one dull mass and
    /// the mass is the message.
    pub kind: CodeKind,
    /// The full composition, not just the winner. A quarter that is 60 % test
    /// and 40 % source is a real thing and the renderer may want to show it.
    pub mix: KindMix,
    /// Files this neighborhood owns — those under [`Neighborhood::path`] that no
    /// *deeper* neighborhood owns. This is the number the map draws.
    pub file_count: u32,
    /// Files under [`Neighborhood::path`] at any depth, deeper neighborhoods
    /// included.
    pub subtree_file_count: u32,
    /// Total size of the files it owns. PRD §7.3 sizes a footprint from
    /// `sqrt(bytes)`; this is the district-level equivalent.
    pub total_size_bytes: u64,
    /// Depth of [`Neighborhood::path`] in components. `0` for the root.
    pub depth: u32,
    /// The enclosing neighborhood, if any. Not the parent *directory* — the
    /// parent *neighborhood*, which may be several directories up.
    pub parent: Option<LogicalPath>,
    /// Neighborhoods immediately inside this one, in path order.
    pub children: Vec<LogicalPath>,
    /// The directory this neighborhood was named from before single-child
    /// collapsing, when that differs from [`Neighborhood::path`].
    ///
    /// `src` for a neighborhood at `src/lib/core` in a tree where `src` and
    /// `src/lib` hold nothing else. Kept so the drill-down can show the real
    /// path and the map does not have to.
    pub collapsed_from: Option<LogicalPath>,
    /// The most-imported file inside, when there is one — PRD §8's monument
    /// signal, reused as evidence for a description.
    pub monument: Option<LogicalPath>,
    /// What it does, when the repository says. `None` is a real answer: a wrong
    /// or generic description is worse than none.
    pub description: Option<Description>,
    /// How current a model-written description is (PRD §12,
    /// [`crate::llm::Freshness`]).
    ///
    /// [`Freshness::Missing`] for every description that is a quotation from the
    /// repository — those are re-derived from the file on every run and cannot
    /// go stale in this sense. It is only ever anything else when
    /// [`Neighborhoods::apply_model_descriptions`] has put a model's words here.
    ///
    /// **A renderer must not draw [`Freshness::Stale`] the same as
    /// [`Freshness::Fresh`].** A caption reading "Payment processing" over a
    /// directory that quietly became the notification service is worse than no
    /// caption: it is confidently wrong, and the operator would trust it.
    ///
    /// `#[serde(default)]` so a cache or a corpus written before this field
    /// existed still loads.
    #[serde(default)]
    pub freshness: Freshness,
}

impl Neighborhood {
    /// True when this is PRD §8's industrial zone — drawn as one mass.
    pub fn is_industrial(&self) -> bool {
        self.kind.is_industrial()
    }

    /// True when a model wrote this neighborhood's description.
    pub fn has_model_description(&self) -> bool {
        self.description.as_ref().is_some_and(Description::is_model)
    }

    /// True when the description on show has drifted from the district
    /// (PRD §12). See [`Neighborhood::freshness`].
    pub fn is_stale(&self) -> bool {
        self.freshness.is_stale()
    }

    /// True when a human wrote this neighborhood's description.
    pub fn has_prose(&self) -> bool {
        self.description.as_ref().is_some_and(Description::is_prose)
    }

    /// The label to draw under the name, if any.
    pub fn label(&self) -> Option<&str> {
        self.description.as_ref().map(|d| d.label.as_str())
    }
}

/// Every neighborhood of one repository, and the numbers behind them.
///
/// Iterated in [`LogicalPath`] order, which is the same on every machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Neighborhoods {
    /// In path order.
    order: Vec<Neighborhood>,
    /// Path to index into [`Neighborhoods::order`].
    by_path: BTreeMap<LogicalPath, usize>,
    /// What the partition did.
    stats: NeighborhoodStats,
    /// Filename stems per neighborhood, and how often each occurs.
    ///
    /// Scaffolding for the inventory synthesiser and worthless to a renderer, so
    /// it is not serialized — which also keeps a serialized `Neighborhoods`
    /// comparable byte for byte without carrying a map that is only ever read
    /// once.
    #[serde(skip)]
    stems: BTreeMap<LogicalPath, BTreeMap<String, u32>>,
}

/// What one pass produced, for the report and for the status bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeighborhoodStats {
    /// Files seen.
    pub total_files: u32,
    /// Files outside every industrial tree — the sizing budget.
    pub civic_files: u32,
    /// Files inside an industrial tree.
    pub vendored_files: u32,
    /// Neighborhoods produced.
    pub districts: u32,
    /// How many of those are industrial.
    pub vendored_districts: u32,
    /// The derived floor: a child below this is not promoted to its own
    /// neighborhood, because it would be too small to label.
    pub min_files: u32,
    /// The derived ceiling: a neighborhood above this is split if it can be.
    pub max_files: u32,
    /// The whole repository's composition.
    pub mix: KindMix,
    /// Neighborhoods with any description at all.
    pub described: u32,
    /// Neighborhoods whose description a human wrote — the honest number.
    pub described_prose: u32,
    /// Neighborhoods whose description a model wrote (PRD §12, [`crate::llm`]).
    ///
    /// Counted separately from [`NeighborhoodStats::described_prose`] on
    /// purpose: the prose fraction measures how well the *repository* documents
    /// itself, and mixing paid-for sentences into it would destroy the only
    /// measurement of the thing the extractor exists to find.
    #[serde(default)]
    pub described_model: u32,
    /// What the description pass cost and threw away.
    pub describe: DescribeStats,
}

impl NeighborhoodStats {
    /// The fraction of neighborhoods with a human-written description.
    #[allow(clippy::cast_possible_truncation)] // a ratio in `0.0..=1.0`, displayed
    pub fn prose_coverage(&self) -> f32 {
        if self.districts == 0 {
            return 0.0;
        }
        (f64::from(self.described_prose) / f64::from(self.districts)) as f32
    }
}

impl Neighborhoods {
    /// Every neighborhood, in path order.
    pub fn all(&self) -> &[Neighborhood] {
        &self.order
    }

    /// One neighborhood by its directory path.
    pub fn get(&self, path: &LogicalPath) -> Option<&Neighborhood> {
        self.by_path.get(path).map(|i| &self.order[*i])
    }

    /// How many there are.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// True when there are none — only possible before a build.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The numbers behind the partition.
    pub fn stats(&self) -> &NeighborhoodStats {
        &self.stats
    }

    /// Which neighborhood a file belongs to: the deepest one whose path is a
    /// prefix of it.
    ///
    /// The rule the whole partition rests on. Because the set of neighborhood
    /// paths is prefix-closed *upwards* to a district that exists — the root is
    /// always one — every file has exactly one owner, and no file can be in two.
    pub fn district_of(&self, file: &LogicalPath) -> Option<&Neighborhood> {
        let mut cursor = file.parent();
        while let Some(dir) = cursor {
            if let Some(found) = self.get(&dir) {
                return Some(found);
            }
            if dir.is_root() {
                break;
            }
            cursor = dir.parent();
        }
        self.get(&LogicalPath::root())
    }

    /// Neighborhoods sorted for a report: most files first, then by path.
    pub fn by_size(&self) -> Vec<&Neighborhood> {
        let mut out: Vec<&Neighborhood> = self.order.iter().collect();
        out.sort_by(|a, b| b.file_count.cmp(&a.file_count).then(a.path.cmp(&b.path)));
        out
    }

    /// Files of each kind across the whole repository.
    pub fn mix(&self) -> &KindMix {
        &self.stats.mix
    }
}

// ---------------------------------------------------------------------------
// Options and configuration
// ---------------------------------------------------------------------------

/// How the partition is drawn.
#[derive(Debug, Clone)]
pub struct NeighborhoodOptions {
    /// A neighborhood holding more than this share of the civic files is split
    /// if it can be.
    ///
    /// `0.08` by default: eight per cent, which guarantees at least a dozen
    /// neighborhoods on any repository with a shape at all and lands in the
    /// twenties-to-forties the brief asks for on the real repositories it was
    /// measured against. It is a *share*, not a count, so it means the same
    /// thing on a hundred files and on ten thousand.
    pub max_share: f32,
    /// A child holding fewer than this share of the civic files is not promoted
    /// to its own neighborhood.
    ///
    /// `0.004`. The scaling half of the floor: on a 5 000-file repository a
    /// 20-file directory is noise, and on a 200-file one it is a quarter of the
    /// map.
    pub min_share: f32,
    /// The absolute floor on a neighborhood, whatever the shares say.
    ///
    /// `6`. A district with three files in it cannot carry a label at the zoom
    /// PRD §12 calls *City*, so promoting it makes the map worse, not finer.
    pub min_files: u32,
    /// How deep the partition may descend. A backstop, not a design parameter:
    /// the floor normally stops the descent long before this does.
    pub max_depth: u32,
    /// Which files are which kind, and which trees are industrial.
    pub rules: KindRules,
}

impl Default for NeighborhoodOptions {
    fn default() -> Self {
        Self {
            max_share: 0.08,
            min_share: 0.004,
            min_files: 6,
            max_depth: 8,
            rules: KindRules::shipped(),
        }
    }
}

impl NeighborhoodOptions {
    /// The two absolute thresholds, derived from the repository's own size.
    ///
    /// Returns `(min_files, max_files)`. Both are computed once, before the
    /// walk, from the **civic** file count — the total with every industrial
    /// tree removed. That subtraction is the difference between a repository
    /// whose own code is partitioned properly and one whose scale was set by
    /// somebody else's `node_modules`.
    pub fn bounds(&self, civic_files: u32) -> (u32, u32) {
        let min = self
            .min_files
            .max(share_of(civic_files, self.min_share))
            .max(1);
        let ceiling = share_of(civic_files, self.max_share);
        // The ceiling can never be below the floor, or a neighborhood would be
        // both too big to keep and too small to split.
        (min, ceiling.max(min))
    }

    /// Options from a configuration file.
    pub fn from_config(config: &NeighborhoodConfig) -> Self {
        let defaults = Self::default();
        Self {
            max_share: config.max_share.unwrap_or(defaults.max_share),
            min_share: config.min_share.unwrap_or(defaults.min_share),
            min_files: config.min_files.unwrap_or(defaults.min_files),
            max_depth: config.max_depth.unwrap_or(defaults.max_depth),
            rules: KindRules::from_config(&config.kinds),
        }
    }
}

/// `count * share`, rounded up.
///
/// The rounding carries a relative slack of one part in a million because the
/// shares are `f32`: `0.004` is not representable, `10_000 * 0.004` is
/// `40.000002`, and a plain `ceil` would make the floor 41 on a repository whose
/// arithmetic says 40. A threshold that depends on a float's last bit is a
/// threshold that can move between compilers, which PRD §7.4 does not allow.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
// Clamped at zero above, and the product of two values a repository can hold.
fn share_of(count: u32, share: f32) -> u32 {
    let value = f64::from(count) * f64::from(share);
    let slack = value.abs() * 1e-6;
    (value - slack).ceil().max(0.0) as u32
}

/// The on-disk configuration, read from `<repo>/.polis/neighborhoods.json`.
///
/// Every field is optional; an absent one keeps the default. The path is inside
/// [`crate::tree::default_walk_exclusions`], so configuring Polis never adds a
/// building to the city — the trap ADR-0065 closed, respected here rather than
/// re-opened.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NeighborhoodConfig {
    /// Overrides [`NeighborhoodOptions::max_share`].
    pub max_share: Option<f32>,
    /// Overrides [`NeighborhoodOptions::min_share`].
    pub min_share: Option<f32>,
    /// Overrides [`NeighborhoodOptions::min_files`].
    pub min_files: Option<u32>,
    /// Overrides [`NeighborhoodOptions::max_depth`].
    pub max_depth: Option<u32>,
    /// The kind rules overlay.
    pub kinds: KindRulesConfig,
}

/// Where a repository's neighborhood configuration lives.
pub fn config_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".polis").join("neighborhoods.json")
}

impl NeighborhoodConfig {
    /// Reads a repository's configuration, if it has one.
    ///
    /// Missing is the defaults; malformed is a warning on stderr and the
    /// defaults. Never a failure — a typo in a preferences file must not stop a
    /// city being drawn.
    pub fn for_repo(repo_root: &Path) -> Self {
        let path = config_path(repo_root);
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!(
                        "polis: {} is not valid neighborhood configuration ({error}); using defaults",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                eprintln!(
                    "polis: cannot read {} ({error}); using defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The directory aggregate
// ---------------------------------------------------------------------------

/// One directory, while the partition is being decided.
#[derive(Debug, Default, Clone)]
struct Dir {
    /// Files directly inside that are not in an industrial tree.
    direct_civic: u32,
    /// Civic files anywhere below.
    subtree_civic: u32,
    /// Immediate child directories.
    children: BTreeSet<LogicalPath>,
    /// This directory is inside (or is) an industrial tree.
    industrial: bool,
    /// This directory is industrial and its parent is not — the mass's edge.
    industrial_root: bool,
    /// The kinds of every civic file below. Needed by the contrast rule in
    /// [`choose`], which is the second reason to descend into a directory.
    mix: KindMix,
}

/// Builds the directory aggregate over a set of files.
///
/// `kinds` is parallel to `files`: the kind of each file, computed once by the
/// caller because it is wanted twice.
fn directories(
    files: &[&FileMeta],
    kinds: &[CodeKind],
    rules: &KindRules,
) -> BTreeMap<LogicalPath, Dir> {
    let mut dirs: BTreeMap<LogicalPath, Dir> = BTreeMap::new();
    dirs.entry(LogicalPath::root()).or_default();
    for (index, meta) in files.iter().enumerate() {
        let parent = meta.path.parent().unwrap_or_else(LogicalPath::root);
        // The parent, then every ancestor, ending at the root.
        let mut chain = Vec::new();
        let mut cursor = Some(parent);
        while let Some(current) = cursor {
            let is_root = current.is_root();
            cursor = if is_root { None } else { current.parent() };
            chain.push(current);
        }
        // Create every directory on the chain, marking industrial trees. A
        // directory's industrial flag is a property of its own path, so it is
        // computed once here and never re-derived.
        for dir in &chain {
            if !dirs.contains_key(dir) {
                let industrial = !dir.is_root() && rules.industrial().is_industrial_dir(dir);
                dirs.insert(
                    dir.clone(),
                    Dir {
                        industrial,
                        ..Dir::default()
                    },
                );
            }
        }
        for pair in chain.windows(2) {
            if let Some(parent) = dirs.get_mut(&pair[1]) {
                parent.children.insert(pair[0].clone());
            }
        }
        let civic = !dirs[&chain[0]].industrial;
        if civic {
            let kind = kinds.get(index).copied().unwrap_or(CodeKind::Unknown);
            for (rank, dir) in chain.iter().enumerate() {
                let entry = dirs.get_mut(dir).expect("just inserted");
                entry.subtree_civic = entry.subtree_civic.saturating_add(1);
                entry.mix.push(kind, meta.size_bytes);
                if rank == 0 {
                    entry.direct_civic = entry.direct_civic.saturating_add(1);
                }
            }
        }
    }
    // The mass's edge: industrial with a parent that is not.
    let industrial: Vec<LogicalPath> = dirs
        .iter()
        .filter(|(_, d)| d.industrial)
        .map(|(p, _)| p.clone())
        .collect();
    for path in industrial {
        let parent_industrial = path
            .parent()
            .and_then(|p| dirs.get(&p))
            .is_some_and(|d| d.industrial);
        if let Some(dir) = dirs.get_mut(&path) {
            dir.industrial_root = !parent_industrial;
        }
    }
    dirs
}

/// Collapses a chain of single-child directories.
///
/// `a/b/c` with nothing else in `a` or `b` is one place with three names, and
/// three nested districts for it is exactly the "scatter of tiny ones" this
/// module exists to remove. Stops at a directory that holds files of its own, at
/// a fork, and at an industrial tree — descending *into* the mass would make the
/// enclosing neighborhood vendored, which is a different claim entirely.
fn descend_chain(dirs: &BTreeMap<LogicalPath, Dir>, start: &LogicalPath) -> LogicalPath {
    let mut path = start.clone();
    loop {
        let Some(node) = dirs.get(&path) else {
            return path;
        };
        if node.industrial || node.direct_civic > 0 {
            return path;
        }
        let occupied: Vec<&LogicalPath> = node
            .children
            .iter()
            .filter(|c| {
                dirs.get(*c)
                    .is_some_and(|d| d.subtree_civic > 0 || d.industrial)
            })
            .collect();
        match occupied.as_slice() {
            [only] if !dirs[*only].industrial => path = (*only).clone(),
            _ => return path,
        }
    }
}

/// Chooses the set of directories that become neighborhoods.
///
/// Industrial trees are collected first and unconditionally — PRD §8 draws each
/// as one mass whatever the rest of the partition decides — and the civic
/// recursion then never has to think about them, because a file inside one is
/// owned by the deeper district by the same prefix rule as everything else.
fn choose(
    dirs: &BTreeMap<LogicalPath, Dir>,
    min_files: u32,
    max_files: u32,
    max_depth: u32,
) -> BTreeSet<LogicalPath> {
    let mut out: BTreeSet<LogicalPath> = dirs
        .iter()
        .filter(|(_, d)| d.industrial_root)
        .map(|(p, _)| p.clone())
        .collect();

    let mut stack = vec![LogicalPath::root()];
    while let Some(start) = stack.pop() {
        let path = descend_chain(dirs, &start);
        let Some(node) = dirs.get(&path) else {
            continue;
        };
        if node.industrial {
            // Already emitted above; nothing civic below it.
            continue;
        }
        let depth = u32::try_from(path.depth()).unwrap_or(u32::MAX);
        if node.subtree_civic <= max_files || depth >= max_depth {
            // Small enough to keep — unless a child of a *different kind* is
            // large enough to label. Colour is the whole point of the exercise:
            // a district drawn in one hue that is half somebody's translation
            // catalogues and half Python is a district whose colour lies. So
            // the second reason to descend is contrast, not size.
            //
            // Measured on Django, where it is the difference between ten
            // `contrib` apps reading as `data` — 2 456 `.po` files outvoting the
            // code — and reading as the source packages they are, with their
            // `locale` trees beside them as their own neighborhoods.
            let mut contrast: Vec<&LogicalPath> = Vec::new();
            if depth < max_depth {
                for child in &node.children {
                    let Some(kid) = dirs.get(child) else { continue };
                    if kid.industrial || kid.subtree_civic < min_files {
                        continue;
                    }
                    // Compared against the district **without** this child, not
                    // against the district as it stands. Comparing against the
                    // whole is the trap: a child big enough to decide its
                    // parent's kind always agrees with it, so the one case worth
                    // splitting is the one case the naive test cannot see.
                    let rest = node.mix.without(&kid.mix);
                    if !rest.is_empty() && rest.dominant() != kid.mix.dominant() {
                        contrast.push(child);
                    }
                }
            }
            if contrast.is_empty() {
                out.insert(path);
                continue;
            }
            let promoted: u32 = contrast
                .iter()
                .map(|c| dirs[*c].subtree_civic)
                .fold(0u32, u32::saturating_add);
            let residual = node.subtree_civic.saturating_sub(promoted);
            for child in contrast {
                stack.push(child.clone());
            }
            // Any residual at all keeps the parent, floor or no floor: a
            // four-file rump beside a correctly-coloured neighbour beats one
            // district wearing the wrong colour, and the four files have to live
            // somewhere regardless.
            if residual > 0 || path.is_root() {
                out.insert(path);
            }
            continue;
        }
        let promotable: Vec<&LogicalPath> = node
            .children
            .iter()
            .filter(|c| {
                dirs.get(*c)
                    .is_some_and(|d| !d.industrial && d.subtree_civic >= min_files)
            })
            .collect();
        let promoted: u32 = promotable
            .iter()
            .map(|c| dirs[*c].subtree_civic)
            .fold(0u32, u32::saturating_add);
        let residual = node.subtree_civic.saturating_sub(promoted);
        // One promotable child is enough. It has to be: the shape this module
        // exists for is a repository whose code all lives under `src/` beside a
        // lone `README.md`, and a rule that needed two children would look at
        // that, find one child and a one-file residual, and hand back the single
        // enormous district it was asked to break up.
        if promotable.is_empty() {
            out.insert(path);
            continue;
        }
        for child in promotable {
            stack.push(child.clone());
        }
        if residual > 0 || path.is_root() {
            out.insert(path);
        }
    }
    // The root is always a neighborhood when nothing else is, so every file has
    // an owner and an empty repository still has a civic square.
    if out.is_empty() {
        out.insert(LogicalPath::root());
    }
    out
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

/// The shortest readable suffix of each path that no other path shares.
///
/// Two components where the path has two, one where it does not, and wider
/// where that still collides: `polis-repo/src` and `polis-layout/src` both want
/// to be called `src`, so both widen at once. Widening is applied to the whole colliding
/// group rather than to one member of it, which is what makes the result
/// independent of the order the paths arrive in.
fn display_names(paths: &BTreeSet<LogicalPath>) -> BTreeMap<LogicalPath, String> {
    // Two components where there are two, not one. `custom` and `ui` are names
    // that could belong to any repository; `components/custom` and
    // `components/ui` are the names the operator already uses in their editor,
    // in their error messages and in their conversations with the agent — which
    // is the addressing system PRD §9 says the tree already owns. The extra
    // component costs about eight characters and is the difference between a
    // label and a guess.
    let mut widths: BTreeMap<&LogicalPath, usize> = paths
        .iter()
        .map(|p| {
            let two = p.depth().clamp(1, 2);
            // …but not when the parent component is a forty-character bucket
            // name. Past `NAME_SOFT_MAX` the context stops being context and
            // starts being the reason the label does not fit on the map.
            if two == 2 && suffix(p, 2).chars().count() > NAME_SOFT_MAX {
                (p, 1)
            } else {
                (p, two)
            }
        })
        .collect();
    let max_width = paths.iter().map(LogicalPath::depth).max().unwrap_or(1);
    for _ in 0..max_width {
        let mut groups: BTreeMap<String, Vec<&LogicalPath>> = BTreeMap::new();
        for path in paths {
            groups
                .entry(suffix(path, widths[path]))
                .or_default()
                .push(path);
        }
        let mut widened = false;
        for (_, group) in groups.iter().filter(|(_, g)| g.len() > 1) {
            for path in group {
                let width = widths.get_mut(path).expect("every path has a width");
                if *width < path.depth() {
                    *width += 1;
                    widened = true;
                }
            }
        }
        if !widened {
            break;
        }
    }
    paths
        .iter()
        .map(|p| (p.clone(), suffix(p, widths[p])))
        .collect()
}

/// How long a two-component name may be before it is narrowed back to one.
///
/// Not a hard cap: a collision can still push a name past it, because being
/// wrong is worse than being long.
const NAME_SOFT_MAX: usize = 30;

/// The last `n` components of a path, joined. `"/"` for the root.
fn suffix(path: &LogicalPath, n: usize) -> String {
    if path.is_root() {
        return "/".to_owned();
    }
    let parts: Vec<&str> = path.components().collect();
    let start = parts.len().saturating_sub(n.max(1));
    parts[start..].join("/")
}

// ---------------------------------------------------------------------------
// Building
// ---------------------------------------------------------------------------

/// Per-district accumulators, kept only while building.
#[derive(Debug, Default)]
struct Accumulator {
    mix: KindMix,
    bytes: u64,
    files: u32,
    /// File stems and how often each occurs, for the synthesised inventory.
    /// Not kept for industrial districts — a `node_modules` with 40 000 files
    /// would build a 40 000-entry map to answer a question nobody asks about it.
    stems: BTreeMap<String, u32>,
}

impl Neighborhoods {
    /// Partitions a repository into neighborhoods.
    ///
    /// No I/O: this is a pure function of the file list. Descriptions are a
    /// second, separate pass ([`Neighborhoods::describe`]) precisely so the
    /// partition can be tested, golden-filed and replayed from a
    /// [`crate::manifest`] corpus that carries no file content at all.
    pub fn build(tree: &RepoTree, options: &NeighborhoodOptions) -> Self {
        Self::from_files(tree.files.values(), options)
    }

    /// [`Neighborhoods::build`] over any set of files.
    ///
    /// The paths must already be logical — worktree-stripped, PRD §7.6.
    #[allow(clippy::too_many_lines)] // one pass over the files, in one place
    pub fn from_files<'a, I>(files: I, options: &NeighborhoodOptions) -> Self
    where
        I: IntoIterator<Item = &'a FileMeta>,
    {
        // Collected and sorted so the caller's iteration order cannot reach the
        // result even when the caller is not a `BTreeMap`.
        let mut files: Vec<&FileMeta> = files.into_iter().collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));

        let kinds: Vec<CodeKind> = files
            .iter()
            .map(|m| kind_of_with(&m.path, &options.rules))
            .collect();
        let dirs = directories(&files, &kinds, &options.rules);
        let civic_files = dirs
            .get(&LogicalPath::root())
            .map_or(0, |d| d.subtree_civic);
        let (min_files, max_files) = options.bounds(civic_files);
        let chosen = choose(&dirs, min_files, max_files, options.max_depth);
        let names = display_names(&chosen);

        // One pass over the files, assigning each to the deepest chosen
        // directory that is a prefix of it.
        let mut acc: BTreeMap<&LogicalPath, Accumulator> = BTreeMap::new();
        let mut subtree: BTreeMap<&LogicalPath, u32> = BTreeMap::new();
        let mut total_mix = KindMix::new();
        let mut vendored_files = 0u32;
        for (index, meta) in files.iter().enumerate() {
            let kind = kinds[index];
            total_mix.push(kind, meta.size_bytes);
            let owner = owner_of(&chosen, &meta.path);
            let industrial = dirs.get(&owner).is_some_and(|d| d.industrial);
            if industrial {
                vendored_files = vendored_files.saturating_add(1);
            }
            let owner_ref = chosen.get(&owner).expect("owner is a chosen district");
            let entry = acc.entry(owner_ref).or_default();
            entry.mix.push(kind, meta.size_bytes);
            entry.bytes = entry.bytes.saturating_add(meta.size_bytes);
            entry.files = entry.files.saturating_add(1);
            if !industrial {
                if let Some(stem) = file_stem(&meta.path) {
                    *entry.stems.entry(stem).or_insert(0) += 1;
                }
            }
            // Subtree counts: every chosen ancestor, this one included.
            for district in &chosen {
                if meta.path.starts_with(district) {
                    *subtree.entry(district).or_insert(0) += 1;
                }
            }
        }

        let mut order: Vec<Neighborhood> = Vec::with_capacity(chosen.len());
        for path in &chosen {
            let default = Accumulator::default();
            let entry = acc.get(path).unwrap_or(&default);
            let industrial = dirs.get(path).is_some_and(|d| d.industrial);
            let kind = if industrial {
                CodeKind::Vendored
            } else {
                entry.mix.dominant()
            };
            let collapsed_from = uncollapsed(&dirs, &chosen, path);
            order.push(Neighborhood {
                name: names.get(path).cloned().unwrap_or_else(|| suffix(path, 1)),
                kind,
                mix: entry.mix,
                file_count: entry.files,
                subtree_file_count: subtree.get(path).copied().unwrap_or(0),
                total_size_bytes: entry.bytes,
                depth: u32::try_from(path.depth()).unwrap_or(u32::MAX),
                parent: None,
                children: Vec::new(),
                collapsed_from,
                monument: None,
                description: None,
                freshness: Freshness::Missing,
                path: path.clone(),
            });
        }

        let by_path: BTreeMap<LogicalPath, usize> = order
            .iter()
            .enumerate()
            .map(|(i, n)| (n.path.clone(), i))
            .collect();
        let mut me = Self {
            order,
            by_path,
            stats: NeighborhoodStats {
                total_files: u32::try_from(files.len()).unwrap_or(u32::MAX),
                civic_files,
                vendored_files,
                districts: u32::try_from(chosen.len()).unwrap_or(u32::MAX),
                vendored_districts: 0,
                min_files,
                max_files,
                mix: total_mix,
                described: 0,
                described_prose: 0,
                described_model: 0,
                describe: DescribeStats::default(),
            },
            stems: BTreeMap::new(),
        };
        me.link();
        me.stats.vendored_districts = me
            .order
            .iter()
            .filter(|n| n.is_industrial())
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
        // Stems are only needed by the inventory synthesiser, which runs in the
        // describe pass, so they are handed over rather than stored.
        me.stems = acc.into_iter().map(|(p, a)| (p.clone(), a.stems)).collect();
        me
    }

    /// Fills in [`Neighborhood::parent`] and [`Neighborhood::children`].
    fn link(&mut self) {
        let paths: Vec<LogicalPath> = self.order.iter().map(|n| n.path.clone()).collect();
        let mut children: BTreeMap<usize, Vec<LogicalPath>> = BTreeMap::new();
        for (i, path) in paths.iter().enumerate() {
            if path.is_root() {
                continue;
            }
            let mut cursor = path.parent();
            while let Some(dir) = cursor {
                if let Some(parent) = self.by_path.get(&dir).copied() {
                    self.order[i].parent = Some(dir.clone());
                    children.entry(parent).or_default().push(path.clone());
                    break;
                }
                if dir.is_root() {
                    break;
                }
                cursor = dir.parent();
            }
        }
        for (i, mut kids) in children {
            kids.sort();
            self.order[i].children = kids;
        }
    }
}

/// The deepest chosen directory that is a prefix of `file`.
fn owner_of(chosen: &BTreeSet<LogicalPath>, file: &LogicalPath) -> LogicalPath {
    let mut cursor = file.parent();
    while let Some(dir) = cursor {
        if chosen.contains(&dir) {
            return dir;
        }
        if dir.is_root() {
            break;
        }
        cursor = dir.parent();
    }
    LogicalPath::root()
}

/// The directory a neighborhood would have been called before the single-child
/// chain above it was collapsed, when that differs from its path.
fn uncollapsed(
    dirs: &BTreeMap<LogicalPath, Dir>,
    chosen: &BTreeSet<LogicalPath>,
    path: &LogicalPath,
) -> Option<LogicalPath> {
    let mut head = path.clone();
    while let Some(parent) = head.parent() {
        // The root is not a name anybody reads a chain as: `a/b/c` collapsed
        // from the root would report `/`, which says nothing.
        if parent.is_root() || chosen.contains(&parent) {
            break;
        }
        // Only walk up through directories that would have collapsed: no files
        // of their own, and this the only occupied way down.
        if descend_chain(dirs, &parent) != *path {
            break;
        }
        head = parent;
    }
    (head != *path).then_some(head)
}

/// A file's stem, lowercased, with a trailing extension removed.
fn file_stem(path: &LogicalPath) -> Option<String> {
    let name = path.file_name()?;
    let stem = match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    };
    (!stem.is_empty()).then(|| stem.to_ascii_lowercase())
}

// ---------------------------------------------------------------------------
// Descriptions
// ---------------------------------------------------------------------------

impl Neighborhoods {
    /// Records PRD §8's inbound-import degree so a neighborhood knows its own
    /// monument.
    ///
    /// `inbound` is what [`crate::imports::ImportGraph::inbound_counts`]
    /// returns: `(path, count)`, most-imported first. The monument is evidence
    /// for a description as well as a landmark — "most imported here:
    /// `client.ts`" is a fact about a district that its name does not carry.
    pub fn set_monuments(&mut self, inbound: &[(LogicalPath, u32)]) {
        let mut best: BTreeMap<LogicalPath, (u32, LogicalPath)> = BTreeMap::new();
        for (path, count) in inbound {
            if *count < 2 {
                continue;
            }
            let Some(district) = self.district_of(path).map(|n| n.path.clone()) else {
                continue;
            };
            let entry = best.entry(district).or_insert((0, path.clone()));
            // Strictly greater, then the lower path, so a tie resolves the same
            // way on every machine whatever order `inbound` arrived in.
            if *count > entry.0 || (*count == entry.0 && *path < entry.1) {
                *entry = (*count, path.clone());
            }
        }
        for (district, (_, path)) in best {
            if let Some(i) = self.by_path.get(&district).copied() {
                self.order[i].monument = Some(path);
            }
        }
    }

    /// Derives a description for every neighborhood from the checkout.
    ///
    /// Reads only files that are already in the index, only their first
    /// [`crate::describe::READ_LIMIT_BYTES`], and only for non-industrial
    /// neighborhoods — a `node_modules` package's README describes somebody
    /// else's library, and PRD §8 wants the eye to slide off it anyway.
    ///
    /// `cache` is an optional path to a [`crate::describe::DescriptionCache`];
    /// it is read at the start and written at the end.
    pub fn describe(&mut self, root: &Path, index: &RepoTree, cache: Option<&Path>) {
        let exists = |p: &LogicalPath| index.files.contains_key(p);
        let mut describer = match cache {
            Some(path) => Describer::with_cache(root, path),
            None => Describer::new(root),
        };
        let paths: Vec<LogicalPath> = self.order.iter().map(|n| n.path.clone()).collect();
        for (i, path) in paths.iter().enumerate() {
            if self.order[i].is_industrial() {
                continue;
            }
            let name = self.order[i].name.clone();
            let monument = self.order[i].monument.clone();
            let extra: Vec<LogicalPath> = monument.into_iter().collect();
            let found = describer
                .from_readme(path, &name, &exists)
                .or_else(|| describer.from_manifest(path, &exists))
                .or_else(|| describer.from_doc_comment(path, &extra, &exists))
                .or_else(|| self.inventory(i));
            self.order[i].description = found;
        }
        self.stats.describe = *describer.stats();
        if let Some(path) = cache {
            if describer.cache().is_dirty() {
                describer.cache().write(path);
            }
        }
        self.stats.described = self
            .order
            .iter()
            .filter(|n| n.description.is_some())
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
        self.stats.described_prose = self
            .order
            .iter()
            .filter(|n| n.has_prose())
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
    }

    /// Puts a model's words on one neighborhood, with how current they are.
    ///
    /// `pub(crate)` and index-based because the only caller is
    /// [`Neighborhoods::apply_model_descriptions`], which has already decided
    /// that this district wants a model description; letting anything else set
    /// a [`Description`] with [`DescriptionSource::Model`] on it would put text
    /// on the map that no cache entry backs, and therefore text whose
    /// [`crate::llm::Freshness`] means nothing.
    pub(crate) fn set_model_description(
        &mut self,
        index: usize,
        description: Description,
        freshness: Freshness,
    ) {
        if let Some(hood) = self.order.get_mut(index) {
            hood.description = Some(description);
            hood.freshness = freshness;
        }
    }

    /// Records how many neighborhoods a model described.
    pub(crate) fn set_model_described(&mut self, count: u32) {
        self.stats.described_model = count;
        self.stats.described = self
            .order
            .iter()
            .filter(|n| n.description.is_some())
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
    }

    /// True when naming the monument tells the reader something the map has not
    /// already told them.
    ///
    /// Three ways it does not, all found by reading the output on the operator's
    /// own repositories rather than by reasoning about it:
    ///
    /// 1. **It repeats the district's leaf name.** `components/ChatWindow`
    ///    described as "most imported: `ChatWindow.jsx`" is the label twice.
    ///    [`crate::describe::says_nothing_new`] alone misses this, because it
    ///    compares against the *display name* — `components/ChatWindow` — which
    ///    a leaf name never equals. Compare against the last path component too.
    /// 2. **The extension is doing the work.** `ChatWindow` and `ChatWindow.jsx`
    ///    fold to different strings, so the stem is what gets compared.
    /// 3. **The name is a universal entry point.** "most imported: `index.ts`"
    ///    is true of most directories and distinguishes none of them. PRD §8
    ///    still wants that file drawn as a monument and labelled *as a
    ///    building*; it is a useless label for the *district*.
    fn monument_says_something(n: &Neighborhood, file_name: &str) -> bool {
        const ENTRY_POINT_STEMS: &[&str] = &[
            "index", "main", "mod", "lib", "app", "init", "__init__", "entry", "root",
        ];
        let stem = file_name.rsplit_once('.').map_or(file_name, |(s, _)| s);
        if ENTRY_POINT_STEMS
            .iter()
            .any(|e| e.eq_ignore_ascii_case(stem))
        {
            return false;
        }
        let leaf = n.path.file_name().unwrap_or("/");
        !crate::describe::says_nothing_new(stem, &n.name)
            && !crate::describe::says_nothing_new(stem, leaf)
    }

    /// The last resort: a description synthesised from what is actually there.
    ///
    /// Deliberately a **statement of fact**, never a guess at intent. It is
    /// emitted only when it carries something the map does not already show —
    /// a most-imported file, or a filename stem that repeats across a quarter of
    /// the district — because "utils" described as "Utility functions" is noise,
    /// and the brief's standard is that an empty description beats a filler one.
    fn inventory(&self, i: usize) -> Option<Description> {
        let n = &self.order[i];
        if n.file_count == 0 {
            return None;
        }
        let monument = n
            .monument
            .as_ref()
            .and_then(|p| p.file_name())
            .filter(|name| Self::monument_says_something(n, name));
        let stem = self.repeated_stem(&n.path, n.file_count, &n.name);
        if monument.is_none() && stem.is_none() {
            return None;
        }
        // Only worth saying when it is actually a mixture: "100% source" on a
        // district already coloured for source is the filler this module is
        // supposed to refuse.
        let mixed = n
            .mix
            .ranked()
            .iter()
            .filter(|(k, _)| n.mix.share(*k) >= 0.15)
            .count()
            >= 2;
        let mix = if mixed {
            n.mix.summary(2, 0.15)
        } else {
            String::new()
        };
        let mut label = String::new();
        if let Some(name) = monument {
            label.push_str("most imported: ");
            label.push_str(name);
        } else if let Some((stem, count)) = &stem {
            let _ = write!(label, "{count} files named {stem}*");
        }
        if !mix.is_empty() {
            label.push_str(" · ");
            label.push_str(&mix);
        }
        let mut detail = format!("{} files", n.file_count);
        if !mix.is_empty() {
            detail.push_str(", ");
            detail.push_str(&mix);
        }
        detail.push('.');
        if let Some(path) = &n.monument {
            let _ = write!(detail, " Most imported here: {}.", path.as_str());
        }
        if let Some((stem, count)) = &stem {
            let _ = write!(detail, " {count} files share the name {stem}.");
        }
        Some(Description {
            label: crate::describe::sanitise(&label, crate::describe::LABEL_MAX_CHARS).ok()?,
            detail: crate::describe::sanitise(&detail, crate::describe::DETAIL_MAX_CHARS)
                .unwrap_or_else(|_| label.clone()),
            source: DescriptionSource::Inventory,
            origin: None,
        })
    }

    /// The most common filename stem in a district, when it repeats often enough
    /// to be a fact rather than a coincidence.
    fn repeated_stem(&self, path: &LogicalPath, files: u32, name: &str) -> Option<(String, u32)> {
        let stems = self.stems.get(path)?;
        let (stem, count) = stems
            .iter()
            // Highest count, then the lowest stem: a fixed order on both keys.
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))?;
        if *count < 3 || *count * 4 < files || crate::describe::says_nothing_new(stem, name) {
            return None;
        }
        Some((stem.clone(), *count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_events::WallTime;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    fn meta(path: &str, size: u64) -> FileMeta {
        FileMeta {
            path: lp(path),
            size_bytes: size,
            growth_index: 0,
            added_at: WallTime::UNIX_EPOCH,
            last_touched: WallTime::UNIX_EPOCH,
            class: crate::FileClass::Ordinary,
            language: None,
        }
    }

    fn build(paths: &[String]) -> Neighborhoods {
        let files: Vec<FileMeta> = paths.iter().map(|p| meta(p, 1_000)).collect();
        Neighborhoods::from_files(files.iter(), &NeighborhoodOptions::default())
    }

    fn names(n: &Neighborhoods) -> Vec<String> {
        n.all().iter().map(|d| d.name.clone()).collect()
    }

    fn fan(prefix: &str, count: usize) -> Vec<String> {
        (0..count).map(|i| format!("{prefix}/f{i:03}.rs")).collect()
    }

    // -- the shapes the brief asks about -------------------------------------

    #[test]
    fn a_flat_repository_is_one_neighborhood_and_says_so() {
        let paths: Vec<String> = (0..40).map(|i| format!("f{i:02}.rs")).collect();
        let n = build(&paths);
        assert_eq!(n.len(), 1);
        assert_eq!(names(&n), ["/"]);
        assert_eq!(n.all()[0].file_count, 40);
        assert_eq!(n.all()[0].kind, CodeKind::Source);
    }

    #[test]
    fn a_single_child_chain_collapses_to_one_name() {
        let paths = fan("a/b/c/d", 30);
        let n = build(&paths);
        assert_eq!(n.len(), 1, "{:?}", names(&n));
        assert_eq!(n.all()[0].path.as_str(), "a/b/c/d");
        assert_eq!(n.all()[0].name, "c/d");
        assert_eq!(
            n.all()[0].collapsed_from.as_ref().map(LogicalPath::as_str),
            Some("a")
        );
    }

    #[test]
    fn a_monorepo_gets_one_neighborhood_per_package_at_least() {
        let mut paths = Vec::new();
        for pkg in 0..10 {
            paths.extend(fan(&format!("packages/p{pkg}/src"), 30));
        }
        let n = build(&paths);
        assert!(n.len() >= 10, "{:?}", names(&n));
        // Ten `src` directories cannot all be called `src`.
        let mut names = names(&n);
        let total = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), total, "names must be unique");
        assert!(names.iter().any(|s| s == "p3/src"), "{names:?}");
    }

    #[test]
    fn a_vendored_tree_is_one_mass_and_does_not_set_the_scale() {
        // 60 files of the operator's own code under 3 000 of somebody else's.
        let mut paths = fan("src/auth", 20);
        paths.extend(fan("src/api", 20));
        paths.extend(fan("src/ui", 20));
        for pkg in 0..30 {
            paths.extend(fan(&format!("node_modules/pkg{pkg}/lib"), 100));
        }
        let n = build(&paths);
        let vendored: Vec<&Neighborhood> = n.all().iter().filter(|d| d.is_industrial()).collect();
        assert_eq!(vendored.len(), 1, "one mass, not thirty");
        assert_eq!(vendored[0].path.as_str(), "node_modules");
        assert_eq!(vendored[0].file_count, 3_000);
        assert_eq!(vendored[0].kind, CodeKind::Vendored);
        // And the operator's own 60 files still got partitioned properly, which
        // they would not have if 3 060 had set the ceiling.
        let civic: Vec<&str> = n
            .all()
            .iter()
            .filter(|d| !d.is_industrial())
            .map(|d| d.name.as_str())
            .collect();
        assert!(civic.contains(&"src/auth"), "{civic:?}");
        assert!(civic.contains(&"src/api"), "{civic:?}");
        assert_eq!(n.stats().civic_files, 60);
        assert_eq!(n.stats().vendored_files, 3_000);
    }

    // -- the granularity rule -----------------------------------------------

    #[test]
    fn a_repository_that_lives_under_src_no_longer_gets_one_blob() {
        // The user's actual complaint, as a test.
        let mut paths = Vec::new();
        for area in ["auth", "api", "ui", "store", "render", "wire"] {
            paths.extend(fan(&format!("src/{area}"), 60));
        }
        paths.push("README.md".to_owned());
        let n = build(&paths);
        let names = names(&n);
        assert!(n.len() >= 6, "{names:?}");
        for area in ["auth", "api", "ui", "store", "render", "wire"] {
            let want = format!("src/{area}");
            assert!(names.contains(&want), "{want} missing: {names:?}");
        }
        // And no neighborhood holds more than the ceiling.
        let (_, ceiling) = NeighborhoodOptions::default().bounds(n.stats().civic_files);
        for d in n.all() {
            assert!(
                d.file_count <= ceiling || d.children.is_empty(),
                "{} holds {} of {ceiling}",
                d.name,
                d.file_count
            );
        }
    }

    #[test]
    fn a_child_too_small_to_label_stays_with_its_parent() {
        let mut paths = fan("src/big", 200);
        // Twelve directories of two files each: below the floor, so none of them
        // becomes a district of its own.
        for i in 0..12 {
            paths.extend(fan(&format!("src/tiny{i}"), 2));
        }
        let n = build(&paths);
        let names = names(&n);
        assert!(!names.iter().any(|s| s.contains("tiny")), "{names:?}");
        let residual = n.get(&lp("src")).expect("a residual district for src");
        assert_eq!(residual.file_count, 24);
    }

    #[test]
    fn the_bounds_scale_with_the_repository() {
        let opts = NeighborhoodOptions::default();
        assert_eq!(opts.bounds(0), (6, 6));
        assert_eq!(opts.bounds(100), (6, 8));
        assert_eq!(opts.bounds(1_000), (6, 80));
        assert_eq!(opts.bounds(10_000), (40, 800));
        // The ceiling is never below the floor.
        let (min, max) = opts.bounds(3);
        assert!(max >= min);
    }

    // -- the invariants ------------------------------------------------------

    #[test]
    fn every_file_has_exactly_one_neighborhood() {
        let mut paths = fan("src/a", 40);
        paths.extend(fan("src/a/deep/deeper", 40));
        paths.extend(fan("docs", 20));
        paths.extend(fan("node_modules/x", 500));
        paths.push("README.md".to_owned());
        let files: Vec<FileMeta> = paths.iter().map(|p| meta(p, 10)).collect();
        let n = Neighborhoods::from_files(files.iter(), &NeighborhoodOptions::default());
        let mut counted = 0u32;
        for file in &files {
            let owner = n.district_of(&file.path).expect("every file has an owner");
            assert!(file.path.starts_with(&owner.path), "{}", file.path.as_str());
            counted += 1;
        }
        assert_eq!(counted as usize, files.len());
        let summed: u32 = n.all().iter().map(|d| d.file_count).sum();
        assert_eq!(summed as usize, files.len(), "file counts must partition");
        assert_eq!(n.stats().mix.total() as usize, files.len());
    }

    #[test]
    fn the_partition_does_not_depend_on_input_order() {
        let mut paths = fan("src/a", 30);
        paths.extend(fan("src/b", 30));
        paths.extend(fan("tests", 25));
        paths.extend(fan("node_modules/p", 90));
        let files: Vec<FileMeta> = paths.iter().map(|p| meta(p, 7)).collect();
        let forward = Neighborhoods::from_files(files.iter(), &NeighborhoodOptions::default());
        let backward =
            Neighborhoods::from_files(files.iter().rev(), &NeighborhoodOptions::default());
        assert_eq!(
            serde_json::to_string(&forward).expect("json"),
            serde_json::to_string(&backward).expect("json"),
            "input permutation must not move a neighborhood"
        );
        // And twice in the same process gives the same bytes.
        let again = Neighborhoods::from_files(files.iter(), &NeighborhoodOptions::default());
        assert_eq!(
            serde_json::to_string(&forward).expect("json"),
            serde_json::to_string(&again).expect("json")
        );
    }

    #[test]
    fn names_are_unique_and_as_short_as_they_can_be() {
        let mut paths = fan("polis-repo/src", 40);
        paths.extend(fan("polis-layout/src", 40));
        paths.extend(fan("docs", 30));
        let n = build(&paths);
        let names = names(&n);
        assert!(names.contains(&"docs".to_owned()), "{names:?}");
        assert!(names.contains(&"polis-repo/src".to_owned()), "{names:?}");
        assert!(names.contains(&"polis-layout/src".to_owned()), "{names:?}");
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
    }

    #[test]
    fn the_kind_is_the_dominant_kind_and_the_mix_survives() {
        let mut files: Vec<FileMeta> = Vec::new();
        for i in 0..12 {
            files.push(meta(&format!("app/test_{i}.py"), 100));
        }
        for i in 0..8 {
            files.push(meta(&format!("app/view{i}.py"), 100));
        }
        let n = Neighborhoods::from_files(files.iter(), &NeighborhoodOptions::default());
        let app = n.get(&lp("app")).expect("app");
        assert_eq!(app.kind, CodeKind::Test);
        assert_eq!(app.mix.count(CodeKind::Test), 12);
        assert_eq!(app.mix.count(CodeKind::Source), 8);
        assert_eq!(app.mix.summary(2, 0.1), "60% test, 40% source");
    }

    #[test]
    fn parents_and_children_are_neighborhoods_not_directories() {
        let mut paths = fan("src/a/b/c", 40);
        paths.extend(fan("src/d", 40));
        paths.push("src/root.rs".to_owned());
        let n = build(&paths);
        let child = n
            .all()
            .iter()
            .find(|d| d.path.as_str().starts_with("src/a"))
            .expect("a nested district");
        // Its parent is `src`, which is a neighborhood; `src/a` and `src/a/b`
        // are directories but not neighborhoods.
        assert_eq!(child.parent.as_ref().map(LogicalPath::as_str), Some("src"));
        let src = n.get(&lp("src")).expect("src");
        assert!(src.children.contains(&child.path));
    }

    #[test]
    fn an_empty_repository_still_has_a_civic_square() {
        let n = Neighborhoods::from_files(std::iter::empty(), &NeighborhoodOptions::default());
        assert_eq!(n.len(), 1);
        assert_eq!(n.all()[0].path, LogicalPath::root());
        assert_eq!(n.all()[0].file_count, 0);
        assert_eq!(n.all()[0].kind, CodeKind::Unknown);
    }

    // -- monuments and the inventory ----------------------------------------

    #[test]
    fn a_monument_is_the_most_imported_file_in_its_own_neighborhood() {
        let mut paths = fan("src/api", 40);
        paths.extend(fan("src/ui", 40));
        let mut n = build(&paths);
        n.set_monuments(&[
            (lp("src/api/f001.rs"), 12),
            (lp("src/api/f002.rs"), 12),
            (lp("src/ui/f005.rs"), 3),
            (lp("src/ui/f000.rs"), 1),
        ]);
        let api = n.get(&lp("src/api")).expect("api");
        // A tie resolves on the lower path, on every machine.
        assert_eq!(
            api.monument.as_ref().map(LogicalPath::as_str),
            Some("src/api/f001.rs")
        );
        let ui = n.get(&lp("src/ui")).expect("ui");
        assert_eq!(
            ui.monument.as_ref().map(LogicalPath::as_str),
            Some("src/ui/f005.rs")
        );
    }

    #[test]
    fn the_inventory_stays_quiet_when_it_has_nothing_to_add() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = fan("utils", 30);
        let files: Vec<FileMeta> = paths.iter().map(|p| meta(p, 10)).collect();
        let mut tree = RepoTree::default();
        for f in &files {
            tree.files.insert(f.path.clone(), f.clone());
        }
        let mut n = Neighborhoods::from_files(files.iter(), &NeighborhoodOptions::default());
        n.describe(dir.path(), &tree, None);
        // Thirty files with unrelated names, no imports, no README: there is
        // nothing true to say, so nothing is said.
        assert_eq!(n.get(&lp("utils")).expect("utils").description, None);
        assert_eq!(n.stats().described, 0);
    }

    #[test]
    fn the_inventory_speaks_when_it_has_a_fact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = fan("api", 30);
        let files: Vec<FileMeta> = paths.iter().map(|p| meta(p, 10)).collect();
        let mut tree = RepoTree::default();
        for f in &files {
            tree.files.insert(f.path.clone(), f.clone());
        }
        let mut n = Neighborhoods::from_files(files.iter(), &NeighborhoodOptions::default());
        n.set_monuments(&[(lp("api/f007.rs"), 19)]);
        n.describe(dir.path(), &tree, None);
        let api = n.get(&lp("api")).expect("api");
        let description = api.description.as_ref().expect("an inventory");
        assert_eq!(description.source, DescriptionSource::Inventory);
        assert!(description.label.contains("f007.rs"), "{description:?}");
        assert!(!description.is_prose(), "an inventory is not prose");
        assert_eq!(n.stats().described, 1);
        assert_eq!(n.stats().described_prose, 0);
    }

    /// `components/ChatWindow` described as "most imported: `ChatWindow.jsx`"
    /// is the label printed twice, and it happened on four districts of
    /// `qurio-toolset` and `biwt`. So did "most imported: `index.ts`", which is
    /// true of most directories and tells the operator apart from none of them.
    #[test]
    fn a_monument_that_only_repeats_the_district_name_is_not_a_description() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (district, monument) in [
            // The leaf name, with an extension on it.
            (
                "components/ChatWindow",
                "components/ChatWindow/ChatWindow.jsx",
            ),
            // A universal entry point.
            ("components/ChatWindow", "components/ChatWindow/index.ts"),
        ] {
            let paths = fan(district, 30);
            let files: Vec<FileMeta> = paths.iter().map(|p| meta(p, 10)).collect();
            let mut tree = RepoTree::default();
            for f in &files {
                tree.files.insert(f.path.clone(), f.clone());
            }
            let mut n = Neighborhoods::from_files(files.iter(), &NeighborhoodOptions::default());
            n.set_monuments(&[(lp(monument), 19)]);
            n.describe(dir.path(), &tree, None);
            let hood = n.get(&lp(district)).expect("the district");
            assert!(
                hood.description.is_none(),
                "{monument} described {district} as {:?}",
                hood.description
            );
            // The monument itself is untouched: PRD §8 still labels that
            // building. It is only useless as a label for the district.
            assert_eq!(hood.monument.as_ref(), Some(&lp(monument)));
        }
    }

    #[test]
    fn a_readme_beats_the_inventory_and_counts_as_prose() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("api")).expect("mkdir");
        std::fs::write(
            root.join("api/README.md"),
            "# api\n\nRoutes every inbound webhook to the handler that claims it.\n",
        )
        .expect("write");
        let mut paths = fan("api", 30);
        paths.push("api/README.md".to_owned());
        let files: Vec<FileMeta> = paths.iter().map(|p| meta(p, 10)).collect();
        let mut tree = RepoTree::default();
        for f in &files {
            tree.files.insert(f.path.clone(), f.clone());
        }
        let mut n = Neighborhoods::from_files(files.iter(), &NeighborhoodOptions::default());
        n.set_monuments(&[(lp("api/f007.rs"), 19)]);
        n.describe(root, &tree, None);
        let api = n.get(&lp("api")).expect("api");
        let description = api.description.as_ref().expect("a README");
        assert_eq!(description.source, DescriptionSource::Readme);
        assert!(
            description.label.starts_with("Routes every inbound"),
            "{description:?}"
        );
        assert_eq!(n.stats().described_prose, 1);
        assert!((n.stats().prose_coverage() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_vendored_neighborhood_is_never_described() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("node_modules/react")).expect("mkdir");
        std::fs::write(
            root.join("node_modules/react/README.md"),
            "# react\n\nA JavaScript library for building user interfaces.\n",
        )
        .expect("write");
        let mut paths = fan("node_modules/react", 40);
        paths.push("node_modules/react/README.md".to_owned());
        paths.extend(fan("src", 20));
        let files: Vec<FileMeta> = paths.iter().map(|p| meta(p, 10)).collect();
        let mut tree = RepoTree::default();
        for f in &files {
            tree.files.insert(f.path.clone(), f.clone());
        }
        let mut n = Neighborhoods::from_files(files.iter(), &NeighborhoodOptions::default());
        n.describe(root, &tree, None);
        // Somebody else's library does not get to describe a district of this
        // city — PRD §8 wants the eye to slide off it.
        assert_eq!(n.get(&lp("node_modules")).expect("mass").description, None);
    }

    // -- configuration -------------------------------------------------------

    #[test]
    fn configuration_moves_the_thresholds_and_the_rules() {
        let config = NeighborhoodConfig {
            max_share: Some(0.5),
            min_files: Some(100),
            ..NeighborhoodConfig::default()
        };
        let options = NeighborhoodOptions::from_config(&config);
        let mut paths = fan("src/a", 40);
        paths.extend(fan("src/b", 40));
        let files: Vec<FileMeta> = paths.iter().map(|p| meta(p, 1)).collect();
        let n = Neighborhoods::from_files(files.iter(), &options);
        // Nothing is big enough to split against a 50 % ceiling with a floor of
        // a hundred, so the whole repository is one collapsed neighborhood.
        assert_eq!(n.len(), 1, "{:?}", names(&n));
        assert_eq!(n.all()[0].path.as_str(), "src");
    }

    #[test]
    fn a_missing_or_broken_config_falls_back_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            NeighborhoodConfig::for_repo(dir.path()),
            NeighborhoodConfig::default()
        );
        std::fs::create_dir_all(dir.path().join(".polis")).expect("mkdir");
        std::fs::write(config_path(dir.path()), b"{ not json").expect("write");
        assert_eq!(
            NeighborhoodConfig::for_repo(dir.path()),
            NeighborhoodConfig::default()
        );
    }

    #[test]
    fn the_config_round_trips_through_json() {
        let config = NeighborhoodConfig {
            max_share: Some(0.12),
            min_files: Some(8),
            ..NeighborhoodConfig::default()
        };
        let text = serde_json::to_string(&config).expect("serialize");
        let back: NeighborhoodConfig = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(config, back);
    }
}
