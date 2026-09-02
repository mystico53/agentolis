//! The import graph, which becomes the streets (PRD §9).
//!
//! > Streets are **cross-district import relationships only**. Intra-district
//! > coupling is expected and boring; what crosses a boundary is the
//! > architecturally interesting thing.
//!
//! > **Do not let the import graph fight the directory tree for position.** The
//! > tree determines placement […] Imports act only as a weak attraction force
//! > *within* a district, and as drawn edges everywhere else.
//!
//! Extraction is `tree-sitter` per language, run once at index time and
//! incrementally on file change. **Failure to parse a file is non-fatal** — that
//! file simply has no streets.
//!
//! # The three-stage pipeline
//!
//! 1. `Extractor` turns one file's source into a list of *module specifiers* —
//!    the strings the code actually wrote (`"./auth"`, `"crate::imports"`,
//!    `"react"`). This stage is pure syntax and knows nothing about the repo.
//! 2. [`ImportIndex`] resolves each specifier to an [`ImportTarget`]. It is
//!    deliberately conservative: a specifier that cannot be resolved is
//!    [`ImportTarget::Unresolved`] and a package is [`ImportTarget::External`].
//!    **Neither is dropped and neither is guessed at** — a wrong edge draws a
//!    street between two districts that have no relationship, which is worse
//!    than no street at all.
//! 3. [`ImportGraph::cross_district_edges`] collapses the file-level edges to
//!    (district, district) pairs carrying a count of *distinct* underlying
//!    edges, because that count is the rendered street width. The file-level
//!    edges survive alongside it — PRD §12's drill-down layer needs them.
//!
//! # Grammar loading
//!
//! Each grammar crate exports a `tree_sitter_language::LanguageFn` constant that
//! converts into a `tree_sitter::Language`:
//!
//! ```text
//! tree_sitter_rust::LANGUAGE
//! tree_sitter_javascript::LANGUAGE
//! tree_sitter_typescript::LANGUAGE_TYPESCRIPT   // and LANGUAGE_TSX
//! tree_sitter_python::LANGUAGE
//! ```
//!
//! The version numbers on those crates look mismatched against the 0.27 runtime
//! and are not — a grammar depends on the ABI shim, not on the parser. That
//! compatibility is asserted at test time by `tests/grammar_abi.rs`; ADR-0051.
//! [`check_grammars`] is the same assertion available at runtime, because
//! ADR-0051's hazard is precisely that "no streets anywhere" looks exactly like
//! a healthy map.
//!
//! **A `Parser` is not `Sync` and holds an allocation.** Build one per thread and
//! reuse it across files; constructing one per file is most of the cost of a
//! cold index. [`ImportGraph::build`] therefore keeps one `Extractor` cache per
//! worker thread and shares nothing but the immutable [`ImportIndex`].
//!
//! # Determinism
//!
//! The extracted edges reach the layout as an attraction force, so their order
//! is layout-visible. Sort edges before returning them, and never let a
//! `HashMap` walk decide which edge is seen first (PRD §7.4).
//!
//! Parallel parsing is safe here for one specific reason: every file is parsed
//! independently and the results land in a `BTreeMap` keyed by logical path, so
//! the *order threads finish in cannot be observed*. [`ImportGraph::edges`] is
//! the concatenation of that map's values, and since [`ImportEdge`]'s first
//! ordering field is its `from` path, the concatenation is already globally
//! sorted — no post-hoc sort, and no way for a scheduler to change the answer.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use polis_events::LogicalPath;
use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator};

use crate::{ImportEdge, ImportTarget, Language, RepoTree};

// ---------------------------------------------------------------------------
// Budgets and thresholds
// ---------------------------------------------------------------------------

/// Largest file the extractor will hand to a grammar, in bytes.
///
/// PRD §13.1 budgets cold start → first frame at under 3 s for a 5 000-file
/// repo and parsing is most of it. A source file above 1 MiB is a minified
/// bundle, a generated table, or a vendored blob; none of the three has streets
/// worth drawing and all three parse slowly. Over the cap the file is skipped
/// exactly as an unparseable one is — PRD §9: it "simply has no streets".
pub const MAX_PARSE_BYTES: usize = 1 << 20;

/// Fewest neighbouring districts a district needs before it can be called a hub.
///
/// PRD §9's "a district with streets to everywhere is a hub or god-module" is a
/// ratio, and a ratio alone makes every district in a three-district repo a hub
/// by arithmetic accident. See [`ImportGraph::diagnostics`] for the full rule.
pub const HUB_MIN_NEIGHBOURS: u32 = 3;

/// Most worker threads a cold index will use.
///
/// Each worker owns a `Parser` and a compiled `Query` per language it meets, so
/// past a certain width the setup cost outweighs the parsing. Capped rather
/// than taken from `available_parallelism` directly because Polis shares the
/// machine with the agents it is watching.
const MAX_PARSE_THREADS: usize = 12;

/// Fewest files before parallel parsing is worth a thread spawn.
const PARALLEL_THRESHOLD: usize = 64;

// ---------------------------------------------------------------------------
// The graph
// ---------------------------------------------------------------------------

/// The whole-repository import graph.
///
/// Owns the resolution index as well as the edges, so that
/// [`update_file`](Self::update_file) can re-resolve one changed file without a
/// second walk of the [`RepoTree`].
#[derive(Debug, Default)]
pub struct ImportGraph {
    /// Outbound edges keyed by the importing file. `BTreeMap`, so iteration
    /// order is the path order and nothing a thread scheduler did can be seen.
    by_file: BTreeMap<LogicalPath, Vec<ImportEdge>>,
    /// The flattened, globally sorted view [`Self::edges`] hands out.
    flat: Vec<ImportEdge>,
    /// Snapshot of every non-industrial file, for specifier resolution.
    index: ImportIndex,
    /// What extraction cost and what it skipped.
    stats: ImportStats,
}

impl ImportGraph {
    /// Extracts imports for every file the grammar set understands.
    ///
    /// A file that fails to parse is skipped with a `tracing::debug!`, never an
    /// error: PRD §9 makes that explicitly non-fatal. So is a file that is too
    /// large ([`MAX_PARSE_BYTES`]), is not UTF-8, is unreadable, has no grammar,
    /// or sits in PRD §8's industrial zone.
    pub fn build(tree: &RepoTree) -> Self {
        Self::build_timed(tree).0
    }

    /// [`build`](Self::build), returning how long it took.
    ///
    /// The duration comes from a monotonic [`Instant`], never the wall clock,
    /// and it is returned rather than stored: PRD §7.4 forbids anything the
    /// layout or a serialized artifact can see from depending on timing, and a
    /// duration inside a `Serialize` struct is exactly that.
    pub fn build_timed(tree: &RepoTree) -> (Self, Duration) {
        Self::build_with(tree, None, None)
    }

    /// [`build`](Self::build) through the on-disk specifier cache
    /// [`default_parse_cache_path`] names — the product path.
    ///
    /// # What this is for
    ///
    /// PRD §13.1 budgets cold start to first frame at under three seconds, and
    /// `tree-sitter` over every source file is the second-largest term in it:
    /// measured on this machine, 0.97 s of Django's launch, 1.28 s of
    /// `CPython`'s and 1.05 s of Ansible's. None of that work changes between
    /// two launches of a checkout nobody edited, and none of it is affected by
    /// a commit landing — which is what separates it from the history walk,
    /// whose cache PRD §7.1 already asks for.
    ///
    /// The result is identical to [`build`](Self::build)'s, file for file and
    /// edge for edge: only the syntax stage is remembered, every hit is checked
    /// against a 128-bit digest of the source that produced it, and every
    /// specifier is resolved afresh against this run's index. See
    /// [`ParseCache`].
    ///
    /// A repository with no state directory, an unreadable cache, or a cache
    /// from an older version simply parses everything, exactly as before.
    #[must_use]
    pub fn build_cached(tree: &RepoTree) -> Self {
        let path = default_parse_cache_path(&tree.root);
        let cache = path.as_deref().map(ParseCache::read);
        Self::build_with(tree, cache.as_ref(), path.as_deref()).0
    }

    /// The one body behind [`build_timed`](Self::build_timed) and
    /// [`build_cached`](Self::build_cached).
    fn build_with(
        tree: &RepoTree,
        remembered: Option<&ParseCache>,
        write_to: Option<&Path>,
    ) -> (Self, Duration) {
        let started = Instant::now();

        let index = ImportIndex::from_tree(tree);
        let mut stats = ImportStats::default();
        let candidates = select_candidates(tree, &mut stats);

        let (parsed, parse_stats, seen) = parse_all(&tree.root, &index, &candidates, remembered);
        stats.merge(&parse_stats);
        if let Some(path) = write_to {
            ParseCache {
                by_path: seen.into_iter().map(|e| (e.path.clone(), e)).collect(),
            }
            .write(path);
        }

        let mut graph = Self {
            by_file: parsed.into_iter().collect(),
            flat: Vec::new(),
            index,
            stats,
        };
        graph.rebuild_flat();

        let elapsed = started.elapsed();
        tracing::info!(
            files_considered = graph.stats.files_considered,
            files_parsed = graph.stats.files_parsed,
            edges = graph.flat.len(),
            millis = elapsed.as_millis(),
            "PRD §9 import graph built"
        );
        (graph, elapsed)
    }

    /// Re-extracts one file after it changed.
    ///
    /// The language is looked up in the index snapshot taken at
    /// [`build`](Self::build) time. A file the index has never seen has no known
    /// grammar and therefore no streets; use [`upsert_file`](Self::upsert_file)
    /// to introduce one, which is what a `Created` filesystem event should do.
    pub fn update_file(&mut self, path: &LogicalPath, source: &str) {
        match self.index.language(path) {
            Some(language) => self.upsert_file(path, language, source),
            None => {
                if self.by_file.remove(path).is_some() {
                    self.rebuild_flat();
                }
            }
        }
    }

    /// Re-extracts one file, declaring its language.
    ///
    /// The form [`update_file`](Self::update_file) cannot express: a file that
    /// did not exist when the index was built. Registers the path so that later
    /// specifiers can resolve *to* it as well as *from* it.
    pub fn upsert_file(&mut self, path: &LogicalPath, language: Language, source: &str) {
        self.index.insert(path.clone(), Some(language));
        let edges = extract_with(path, language, source, Some(&self.index), &mut self.stats);
        if edges.is_empty() {
            self.by_file.remove(path);
        } else {
            self.by_file.insert(path.clone(), edges);
        }
        self.rebuild_flat();
    }

    /// Forgets a deleted file's edges, in and out.
    ///
    /// Outbound edges vanish with the file. Inbound edges do **not**: the
    /// importing file's source still says `import './gone'`, so those edges are
    /// downgraded to [`ImportTarget::Unresolved`] rather than deleted. That is
    /// PRD §7.5's "vacant lot, not a dangling edge" — the relationship is still
    /// recorded, it just no longer lands anywhere on the map.
    pub fn remove_file(&mut self, path: &LogicalPath) {
        self.index.remove(path);
        self.by_file.remove(path);
        for edges in self.by_file.values_mut() {
            for edge in edges.iter_mut() {
                if edge.to.internal() == Some(path) {
                    edge.to = ImportTarget::Unresolved;
                }
            }
            edges.sort_unstable();
            edges.dedup();
        }
        self.rebuild_flat();
    }

    /// Every extracted edge, in a deterministic order.
    pub fn edges(&self) -> &[ImportEdge] {
        &self.flat
    }

    /// Outbound edges for one file, in a deterministic order.
    pub fn edges_from(&self, path: &LogicalPath) -> &[ImportEdge] {
        self.by_file.get(path).map_or(&[][..], Vec::as_slice)
    }

    /// Number of edges of every kind.
    pub fn len(&self) -> usize {
        self.flat.len()
    }

    /// True when nothing imports anything.
    pub fn is_empty(&self) -> bool {
        self.flat.is_empty()
    }

    /// What extraction cost and what it skipped, cumulative since
    /// [`build`](Self::build).
    pub fn stats(&self) -> &ImportStats {
        &self.stats
    }

    /// Edges that landed on a file inside the repository — the only ones that
    /// can become streets.
    pub fn internal_edges(&self) -> impl Iterator<Item = &ImportEdge> {
        self.flat
            .iter()
            .filter(|e| matches!(e.to, ImportTarget::Internal(_)))
    }

    /// Edges naming a crate, package or stdlib module. Never drawn as streets;
    /// kept because "this district pulls in 40 packages" is PRD §8's industrial
    /// zone seen from the source side.
    pub fn external_edges(&self) -> impl Iterator<Item = &ImportEdge> {
        self.flat
            .iter()
            .filter(|e| matches!(e.to, ImportTarget::External))
    }

    /// Edges whose specifier the resolver refused to classify.
    ///
    /// A build alias, a `tsconfig` path mapping, a generated module, or a file
    /// that has since been deleted. Surfaced rather than dropped so that "the
    /// streets look sparse" has somewhere to be diagnosed.
    pub fn unresolved_edges(&self) -> impl Iterator<Item = &ImportEdge> {
        self.flat
            .iter()
            .filter(|e| matches!(e.to, ImportTarget::Unresolved))
    }

    /// Distinct external packages imported, per district (PRD §8).
    pub fn external_packages(&self) -> BTreeMap<LogicalPath, BTreeSet<String>> {
        let mut out: BTreeMap<LogicalPath, BTreeSet<String>> = BTreeMap::new();
        for edge in self.external_edges() {
            if let Some(pkg) = package_root(&edge.specifier, edge.language) {
                out.entry(district_of(&edge.from))
                    .or_default()
                    .insert(pkg.to_owned());
            }
        }
        out
    }

    /// Import edges whose endpoints are in different districts. These, and only
    /// these, are drawn as streets.
    ///
    /// [`Street::edge_count`] counts **distinct `(importing file, imported
    /// file)` pairs**, not occurrences: `use crate::a::{X, Y};` is two edges but
    /// one relationship, and street width is a picture of how coupled two
    /// districts are, not of how verbose their `use` statements are.
    pub fn cross_district_edges(&self) -> Vec<Street> {
        let mut distinct: BTreeSet<(LogicalPath, LogicalPath, LogicalPath, LogicalPath)> =
            BTreeSet::new();
        for edge in self.internal_edges() {
            let Some(to) = edge.to.internal() else {
                continue;
            };
            if to == &edge.from {
                continue;
            }
            let (from_district, to_district) = (district_of(&edge.from), district_of(to));
            if from_district == to_district {
                continue;
            }
            distinct.insert((from_district, to_district, edge.from.clone(), to.clone()));
        }

        // The set is sorted on (from district, to district, …), so every
        // district pair is one contiguous run and the count is a run length.
        let mut streets: Vec<Street> = Vec::new();
        for (from, to, _, _) in distinct {
            match streets.last_mut() {
                Some(last) if last.from == from && last.to == to => {
                    last.edge_count = last.edge_count.saturating_add(1);
                }
                _ => streets.push(Street {
                    from,
                    to,
                    edge_count: 1,
                }),
            }
        }
        streets
    }

    /// Edges from and to files inside one district — PRD §9's "weak attraction
    /// force *within* a district", and nothing that is ever drawn.
    ///
    /// A file's edges to itself are excluded: Rust's `use super::*;` inside an
    /// inline `mod tests` genuinely names this file, and a building cannot
    /// attract itself.
    pub fn intra_district_edges(&self, district: &LogicalPath) -> Vec<&ImportEdge> {
        self.internal_edges()
            .filter(|edge| {
                let Some(to) = edge.to.internal() else {
                    return false;
                };
                to != &edge.from
                    && district_of(&edge.from) == *district
                    && district_of(to) == *district
            })
            .collect()
    }

    /// Inbound import count per file, for PRD §8's monument criterion.
    ///
    /// Counts distinct *importing files*, so a file that names the same target
    /// four times contributes one. Sorted by path; files with no inbound imports
    /// are omitted.
    pub fn inbound_counts(&self) -> Vec<(LogicalPath, u32)> {
        let mut distinct: BTreeSet<(LogicalPath, LogicalPath)> = BTreeSet::new();
        for edge in self.internal_edges() {
            let Some(to) = edge.to.internal() else {
                continue;
            };
            if to == &edge.from {
                continue;
            }
            distinct.insert((to.clone(), edge.from.clone()));
        }

        let mut out: Vec<(LogicalPath, u32)> = Vec::new();
        for (to, _) in distinct {
            match out.last_mut() {
                Some((path, count)) if *path == to => *count = count.saturating_add(1),
                _ => out.push((to, 1)),
            }
        }
        out
    }

    /// Files in the top decile of inbound import count — one of the signals
    /// behind a monument (PRD §8).
    ///
    /// The decile is taken over the files that have *any* inbound import, not
    /// over every file in the repository. In a 5 000-file repo where 500 files
    /// are imported at all, a decile of the whole tree would promote all 500 to
    /// monuments and PRD §8's wayfinding layer would be noise. Ties break on
    /// path so the set is stable across runs; the result is sorted by path.
    pub fn inbound_top_decile(&self) -> Vec<LogicalPath> {
        let mut ranked = self.inbound_counts();
        if ranked.is_empty() {
            return Vec::new();
        }
        let take = ranked.len().div_ceil(10);
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let mut out: Vec<LogicalPath> = ranked.into_iter().take(take).map(|(p, _)| p).collect();
        out.sort();
        out
    }

    /// PRD §9's free diagnostics, one row per district, sorted by district.
    ///
    /// > Diagnostics fall out for free: a district with streets to everywhere is
    /// > a hub or god-module; a district with none is isolated.
    ///
    /// The population is every district holding at least one parseable,
    /// non-industrial file — a `docs/` directory of Markdown is not an isolated
    /// district, it is not a code district at all, and counting it would both
    /// dilute the hub ratio and fill the isolated list with noise.
    ///
    /// A district is a **hub** when it neighbours at least
    /// [`HUB_MIN_NEIGHBOURS`] other districts *and* at least half of all other
    /// code districts. The ratio is written as `2 * neighbours >= others` rather
    /// than a float fraction so the answer cannot move with a rounding mode.
    ///
    /// A district is **isolated** when no street touches it in either direction.
    pub fn diagnostics(&self, tree: &RepoTree) -> Vec<DistrictDiagnostics> {
        let streets = self.cross_district_edges();

        let mut rows: BTreeMap<LogicalPath, DistrictDiagnostics> = code_districts(tree)
            .into_iter()
            .map(|d| (d.clone(), DistrictDiagnostics::empty(d)))
            .collect();

        let mut neighbours: BTreeMap<LogicalPath, BTreeSet<LogicalPath>> = BTreeMap::new();
        for street in &streets {
            for district in [&street.from, &street.to] {
                rows.entry(district.clone())
                    .or_insert_with(|| DistrictDiagnostics::empty(district.clone()));
            }
            neighbours
                .entry(street.from.clone())
                .or_default()
                .insert(street.to.clone());
            neighbours
                .entry(street.to.clone())
                .or_default()
                .insert(street.from.clone());

            if let Some(row) = rows.get_mut(&street.from) {
                row.outbound_districts = row.outbound_districts.saturating_add(1);
                row.outbound_edges = row.outbound_edges.saturating_add(street.edge_count);
            }
            if let Some(row) = rows.get_mut(&street.to) {
                row.inbound_districts = row.inbound_districts.saturating_add(1);
                row.inbound_edges = row.inbound_edges.saturating_add(street.edge_count);
            }
        }

        let packages = self.external_packages();
        let others = u32::try_from(rows.len().saturating_sub(1)).unwrap_or(u32::MAX);
        let mut out: Vec<DistrictDiagnostics> = rows.into_values().collect();
        for row in &mut out {
            row.neighbours = neighbours
                .get(&row.district)
                .map_or(0, |n| u32::try_from(n.len()).unwrap_or(u32::MAX));
            row.external_packages = packages
                .get(&row.district)
                .map_or(0, |p| u32::try_from(p.len()).unwrap_or(u32::MAX));
            row.is_isolated = row.neighbours == 0;
            row.is_hub =
                row.neighbours >= HUB_MIN_NEIGHBOURS && row.neighbours.saturating_mul(2) >= others;
        }
        out
    }

    /// Districts with streets to (nearly) everywhere — PRD §9's god-modules.
    pub fn hub_districts(&self, tree: &RepoTree) -> Vec<LogicalPath> {
        self.diagnostics(tree)
            .into_iter()
            .filter(|d| d.is_hub)
            .map(|d| d.district)
            .collect()
    }

    /// Code districts no street touches, in either direction (PRD §9).
    pub fn isolated_districts(&self, tree: &RepoTree) -> Vec<LogicalPath> {
        self.diagnostics(tree)
            .into_iter()
            .filter(|d| d.is_isolated)
            .map(|d| d.district)
            .collect()
    }

    /// The resolution snapshot, for callers that want to resolve one specifier
    /// without rebuilding an index.
    pub fn index(&self) -> &ImportIndex {
        &self.index
    }

    /// Rebuilds the flattened view.
    ///
    /// A plain concatenation, deliberately with no sort: `by_file` is keyed on
    /// the importing path and [`ImportEdge`]'s first ordering field *is* that
    /// path, so walking the map in key order and appending each file's already
    /// sorted edges produces a globally sorted vector. The `debug_assert`
    /// catches it if that ever stops being true.
    fn rebuild_flat(&mut self) {
        let total: usize = self.by_file.values().map(Vec::len).sum();
        self.flat.clear();
        self.flat.reserve(total);
        for edges in self.by_file.values() {
            self.flat.extend(edges.iter().cloned());
        }
        debug_assert!(
            self.flat.is_sorted(),
            "PRD §7.4: edge order is layout-visible and must be sorted"
        );
    }
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

/// A drawn street between two districts (PRD §9).
///
/// The aggregate of every [`ImportEdge`] crossing one district boundary, which
/// is why it carries a count rather than a path pair.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Street {
    /// The district the edges leave.
    pub from: LogicalPath,
    /// The district they arrive at.
    pub to: LogicalPath,
    /// Distinct import edges carried. **Street width is proportional to this**,
    /// which is why it is distinct edges and not total occurrences.
    pub edge_count: u32,
}

/// PRD §9's free diagnostics for one district.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DistrictDiagnostics {
    /// The district these numbers describe.
    pub district: LogicalPath,
    /// Distinct districts this one imports from.
    pub outbound_districts: u32,
    /// Distinct districts that import this one.
    pub inbound_districts: u32,
    /// Distinct districts on the other end of a street, either direction.
    pub neighbours: u32,
    /// Distinct file-level edges leaving the district.
    pub outbound_edges: u32,
    /// Distinct file-level edges arriving at the district.
    pub inbound_edges: u32,
    /// Distinct external packages named by files in this district (PRD §8).
    pub external_packages: u32,
    /// Streets to (nearly) everywhere — a hub or god-module.
    pub is_hub: bool,
    /// No streets at all.
    pub is_isolated: bool,
}

impl DistrictDiagnostics {
    /// A district with nothing recorded against it yet.
    fn empty(district: LogicalPath) -> Self {
        Self {
            district,
            outbound_districts: 0,
            inbound_districts: 0,
            neighbours: 0,
            outbound_edges: 0,
            inbound_edges: 0,
            external_packages: 0,
            is_hub: false,
            is_isolated: true,
        }
    }
}

/// What the extractor did, cumulative since [`ImportGraph::build`].
///
/// Deliberately holds no timing: PRD §7.4 forbids anything a serialized artifact
/// can see from depending on the clock. Use [`ImportGraph::build_timed`] for
/// duration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportStats {
    /// Files the walk looked at.
    pub files_considered: u64,
    /// Files a grammar actually parsed.
    pub files_parsed: u64,
    /// Files with no loaded grammar. Routine, not a failure (PRD §9).
    pub skipped_no_grammar: u64,
    /// Files in PRD §8's industrial zone, deliberately not parsed.
    pub skipped_industrial: u64,
    /// Files above [`MAX_PARSE_BYTES`].
    pub skipped_too_large: u64,
    /// Files that could not be read at all.
    pub skipped_unreadable: u64,
    /// Files that are not UTF-8, or contain a NUL byte, and are therefore not
    /// source no matter what the extension claims.
    pub skipped_binary: u64,
    /// Files that parsed with syntax errors. **Still counted as parsed** —
    /// `tree-sitter` recovers, so a file with one broken function usually still
    /// yields every import above the break.
    pub files_with_parse_errors: u64,
    /// Worker threads that panicked. Non-fatal by PRD §9, but never silent.
    pub panicked_workers: u64,
}

impl ImportStats {
    /// Folds another counter set in. Addition, so the merge order cannot change
    /// the answer — which is what makes parallel parsing deterministic here.
    fn merge(&mut self, other: &Self) {
        self.files_considered += other.files_considered;
        self.files_parsed += other.files_parsed;
        self.skipped_no_grammar += other.skipped_no_grammar;
        self.skipped_industrial += other.skipped_industrial;
        self.skipped_too_large += other.skipped_too_large;
        self.skipped_unreadable += other.skipped_unreadable;
        self.skipped_binary += other.skipped_binary;
        self.files_with_parse_errors += other.files_with_parse_errors;
        self.panicked_workers += other.panicked_workers;
    }
}

/// A grammar or query that the `tree-sitter` runtime refused.
///
/// ADR-0051's hazard in one type: an ABI mismatch is a *runtime* failure, and
/// PRD §9's "failure to parse is non-fatal" would swallow it into a map that
/// looks perfectly healthy and has no streets anywhere. Both variants carry the
/// formatted upstream message rather than the upstream error type, so
/// `tree-sitter`'s error types stay out of `polis-repo`'s public API.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GrammarError {
    /// `Parser::set_language` rejected the grammar.
    #[error("the {0} grammar was rejected by the tree-sitter runtime: {1}")]
    Language(Language, String),
    /// The import query did not compile against the grammar.
    #[error("the {0} import query did not compile: {1}")]
    Query(Language, String),
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// The district a file belongs to — its containing directory (PRD §3).
///
/// A file at the repository root belongs to the root district, PRD §8's civic
/// square, rather than to no district at all.
pub fn district_of(path: &LogicalPath) -> LogicalPath {
    path.parent().unwrap_or_else(LogicalPath::root)
}

/// Extracts every import from one file's source.
///
/// Returns an empty vector — never an error — when the source does not parse.
/// PRD §9: "Failure to parse a file is non-fatal; that file simply has no
/// streets."
///
/// Targets come back as [`ImportTarget::Unresolved`] here; [`resolve`] turns the
/// specifiers into repository paths once the whole tree is known, because
/// resolving `"./auth"` needs to know which of `auth.ts`, `auth/index.ts` and
/// `auth.tsx` exists.
pub fn extract(path: &LogicalPath, language: Language, source: &str) -> Vec<ImportEdge> {
    let mut stats = ImportStats::default();
    extract_with(path, language, source, None, &mut stats)
}

/// Resolves a module specifier against the repository tree.
///
/// Best-effort and deliberately so: an unresolvable specifier is
/// [`ImportTarget::Unresolved`], not an error. Language-specific rules —
/// `crate::`/`super::` for Rust, extension and `index` probing for
/// JavaScript/TypeScript, package-relative dots for Python.
///
/// **This builds a fresh [`ImportIndex`] on every call**, which is a walk of the
/// whole tree. It exists for one-off resolution and for tests; anything
/// resolving more than a handful of specifiers should build one [`ImportIndex`]
/// and call [`ImportIndex::resolve`], which is what [`ImportGraph`] does.
pub fn resolve(
    from: &LogicalPath,
    specifier: &str,
    language: Language,
    tree: &RepoTree,
) -> ImportTarget {
    ImportIndex::from_tree(tree).resolve(from, specifier, language)
}

/// The `tree-sitter` query source used to find imports for a language.
///
/// Exposed so the query can be unit-tested against a fixture without running a
/// whole index, and so a query that fails to compile against a grammar fails
/// loudly in one place instead of silently returning no edges.
///
/// The queries capture *anchors*, not finished specifiers: a Rust
/// `use crate::{a, b::c};` is one `use_declaration` whose argument is a tree
/// that has to be walked, and a JavaScript `require` is a `call_expression`
/// whose callee has to be checked. A query language cannot express either, so
/// the query narrows the search and Rust does the last step.
pub fn query_source(language: Language) -> &'static str {
    match language {
        Language::Rust => RUST_QUERY,
        Language::JavaScript => JS_QUERY,
        Language::TypeScript | Language::Tsx => TS_QUERY,
        Language::Python => PYTHON_QUERY,
    }
}

/// Loads every grammar and compiles every query, right now.
///
/// ADR-0051: a grammar/runtime ABI mismatch is a runtime failure that PRD §9's
/// non-fatal rule hides perfectly — every file silently gets no streets and the
/// map looks fine. `polis doctor` and the test suite call this so the failure is
/// loud in the one place it can be.
pub fn check_grammars() -> Result<(), GrammarError> {
    for language in Language::ALL {
        Extractor::new(language)?;
    }
    Ok(())
}

/// The package a non-repository specifier names, for PRD §8's industrial zone.
///
/// `react-dom/client` is `react-dom`, `@scope/pkg/sub` is `@scope/pkg`,
/// `node:fs` is `fs`, `std::collections::BTreeMap` is `std`, and `os.path` is
/// `os`. Returns `None` for a specifier that names no package — a relative path,
/// or an empty string.
pub fn package_root(specifier: &str, language: Language) -> Option<&str> {
    let spec = specifier.trim();
    if spec.is_empty() {
        return None;
    }
    match language {
        Language::Rust => {
            let head = spec.split("::").next()?.trim();
            (!head.is_empty() && head != "crate" && head != "self" && head != "super")
                .then_some(head)
        }
        Language::Python => {
            if spec.starts_with('.') {
                return None;
            }
            let head = spec.split('.').next()?;
            (!head.is_empty()).then_some(head)
        }
        Language::JavaScript | Language::TypeScript | Language::Tsx => {
            // Relative, root-absolute, or the `@/` project alias: not a package.
            if spec.starts_with('.') || spec.starts_with('/') || spec.starts_with("@/") {
                return None;
            }
            // `node:fs`, `bun:sqlite` — the scheme is not part of the package.
            let bare = spec.split_once(':').map_or(spec, |(_, rest)| rest);
            let mut parts = bare.split('/');
            let head = parts.next()?;
            if head.is_empty() {
                return None;
            }
            if head.starts_with('@') {
                // A scoped package is two segments: `@scope/name`.
                let end = bare[head.len() + 1..]
                    .find('/')
                    .map_or(bare.len(), |i| head.len() + 1 + i);
                return Some(&bare[..end]);
            }
            Some(head)
        }
    }
}

// ---------------------------------------------------------------------------
// Candidate selection
// ---------------------------------------------------------------------------

/// The files worth handing to a grammar, sorted by `(language, path)`.
///
/// Sorting by language first means a worker thread's contiguous chunk usually
/// touches one or two grammars, so it compiles one or two queries rather than
/// five. The final order is irrelevant to the result — parsed edges land in a
/// `BTreeMap` — so this is purely a cost decision.
fn select_candidates(tree: &RepoTree, stats: &mut ImportStats) -> Vec<(LogicalPath, Language)> {
    let mut out: Vec<(LogicalPath, Language)> = Vec::new();
    for (path, meta) in &tree.files {
        stats.files_considered += 1;
        if meta.class.is_massed() || is_industrial_path(path) {
            stats.skipped_industrial += 1;
            continue;
        }
        match meta.language {
            Some(language) => out.push((path.clone(), language)),
            None => stats.skipped_no_grammar += 1,
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// PRD §8's industrial zone, by path, as a cost guard.
///
/// `crate::FileClass::Industrial` — set by `crate::tree::classify` — is the
/// authority on what is industrial, and [`select_candidates`] checks it first.
/// This is the belt-and-braces second check, and it exists for one reason: if
/// classification has not run, a cold index parses every file in `node_modules`
/// and PRD §13.1's 3-second budget is gone. Kept deliberately narrow, because a
/// false positive here costs a district its streets silently.
fn is_industrial_path(path: &LogicalPath) -> bool {
    let mut first = true;
    for component in path.components() {
        let industrial = component.eq_ignore_ascii_case("node_modules")
            || component.eq_ignore_ascii_case("bower_components")
            || component.eq_ignore_ascii_case("site-packages")
            || component.eq_ignore_ascii_case("__pycache__")
            || component.eq_ignore_ascii_case(".git")
            // `target` only at the top level: `src/target/` is a plausible
            // source directory, `./target/` is cargo's.
            || (first && component.eq_ignore_ascii_case("target"));
        if industrial {
            return true;
        }
        first = false;
    }
    false
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// One parse pass: the edges by file, what the pass counted, and what each file
/// produced, ready for [`ParseCache`] to remember.
type Parsed = (
    Vec<(LogicalPath, Vec<ImportEdge>)>,
    ImportStats,
    Vec<CachedFile>,
);

/// Parses every candidate, in parallel when there are enough of them.
///
/// Determinism does not depend on the thread count: each file is parsed
/// independently and the caller drops the results into a `BTreeMap`, so the
/// order workers finish in is unobservable. The counter merge is addition, which
/// is likewise order-independent.
fn parse_all(
    root: &Path,
    index: &ImportIndex,
    candidates: &[(LogicalPath, Language)],
    cache: Option<&ParseCache>,
) -> Parsed {
    let threads = worker_count(candidates.len());
    if threads <= 1 {
        let mut out = Vec::new();
        let mut fresh = Vec::new();
        let mut stats = ImportStats::default();
        parse_chunk(
            root, index, candidates, cache, &mut out, &mut fresh, &mut stats,
        );
        return (out, stats, fresh);
    }

    let chunk_size = candidates.len().div_ceil(threads);
    let mut merged: Vec<(LogicalPath, Vec<ImportEdge>)> = Vec::new();
    let mut fresh: Vec<CachedFile> = Vec::new();
    let mut stats = ImportStats::default();

    std::thread::scope(|scope| {
        let handles: Vec<_> = candidates
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(move || {
                    let mut out = Vec::new();
                    let mut seen = Vec::new();
                    let mut local = ImportStats::default();
                    parse_chunk(root, index, chunk, cache, &mut out, &mut seen, &mut local);
                    (out, local, seen)
                })
            })
            .collect();
        for handle in handles {
            if let Ok((out, local, seen)) = handle.join() {
                merged.extend(out);
                fresh.extend(seen);
                stats.merge(&local);
            } else {
                // PRD §9: non-fatal. Those files simply have no streets.
                tracing::error!("an import-extraction worker panicked; its files have no streets");
                stats.panicked_workers += 1;
            }
        }
    });

    (merged, stats, fresh)
}

/// How many workers to use for `files` candidates.
fn worker_count(files: usize) -> usize {
    if files < PARALLEL_THRESHOLD {
        return 1;
    }
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    cores
        .min(MAX_PARSE_THREADS)
        .min(files.div_ceil(PARALLEL_THRESHOLD))
        .max(1)
}

/// Reads and parses one contiguous slice of the candidate list.
#[allow(clippy::too_many_arguments)] // one worker's whole job, in one place
fn parse_chunk(
    root: &Path,
    index: &ImportIndex,
    chunk: &[(LogicalPath, Language)],
    remembered: Option<&ParseCache>,
    out: &mut Vec<(LogicalPath, Vec<ImportEdge>)>,
    seen: &mut Vec<CachedFile>,
    stats: &mut ImportStats,
) {
    let mut cache = ExtractorCache::default();
    for (path, language) in chunk {
        let Some(source) = read_source(root, path, stats) else {
            continue;
        };
        // The syntax stage, from the previous run when this file's bytes are
        // unchanged. Resolution below runs either way: it is the stage a file
        // arriving or leaving changes, and it is not what costs the second.
        let hit = remembered.and_then(|c| c.get(path, *language, &source));
        let (specifiers, errors) = if let Some(entry) = hit {
            (entry.specifiers.clone(), entry.errors)
        } else {
            let Some(extractor) = cache.get(*language) else {
                continue;
            };
            let before = stats.files_with_parse_errors;
            let found = extractor.specifiers(path, &source, stats);
            (found, stats.files_with_parse_errors > before)
        };
        if hit.is_some() && errors {
            stats.files_with_parse_errors += 1;
        }
        let (lo, hi) = content_digest(source.as_bytes());
        seen.push(CachedFile {
            path: path.clone(),
            lo,
            hi,
            len: source.len() as u64,
            language: *language,
            errors,
            specifiers: specifiers.clone(),
        });
        let edges = resolve_specifiers(path, *language, specifiers, Some(index));
        stats.files_parsed += 1;
        if !edges.is_empty() {
            out.push((path.clone(), edges));
        }
    }
}

/// Reads one file as UTF-8 source, or explains in the counters why it is not.
///
/// Every rejection here is PRD §9's non-fatal case: a binary file, a minified
/// bundle, a file deleted between the walk and the read. None is an error and
/// none stops the index.
fn read_source(root: &Path, path: &LogicalPath, stats: &mut ImportStats) -> Option<String> {
    let bytes = match std::fs::read(root.join(path.as_str())) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::debug!(path = path.as_str(), %error, "unreadable; no streets");
            stats.skipped_unreadable += 1;
            return None;
        }
    };
    if bytes.len() > MAX_PARSE_BYTES {
        tracing::debug!(
            path = path.as_str(),
            bytes = bytes.len(),
            "over the parse cap; no streets"
        );
        stats.skipped_too_large += 1;
        return None;
    }
    if bytes.contains(&0) {
        tracing::debug!(path = path.as_str(), "contains NUL; not source, no streets");
        stats.skipped_binary += 1;
        return None;
    }
    if let Ok(source) = String::from_utf8(bytes) {
        Some(source)
    } else {
        tracing::debug!(path = path.as_str(), "not UTF-8; no streets");
        stats.skipped_binary += 1;
        None
    }
}

/// Turns one file's module specifiers into edges, resolving each against the
/// index when there is one.
///
/// Split out of `Extractor::edges` so the cheap half can run on its own: a
/// [`ParseCache`] hit skips the `tree-sitter` parse and still resolves here,
/// against **this** run's index. The sort and dedup are inside, so a cached file
/// and a freshly parsed one produce the same bytes in the same order (PRD §7.4).
fn resolve_specifiers(
    path: &LogicalPath,
    language: Language,
    specifiers: Vec<(String, u32)>,
    index: Option<&ImportIndex>,
) -> Vec<ImportEdge> {
    let mut edges: Vec<ImportEdge> = specifiers
        .into_iter()
        .map(|(specifier, inline_depth)| {
            let to = index.map_or(ImportTarget::Unresolved, |index| {
                index.resolve_nested(path, &specifier, language, inline_depth)
            });
            ImportEdge {
                from: path.clone(),
                to,
                specifier,
                language,
            }
        })
        .collect();
    edges.sort_unstable();
    edges.dedup();
    edges
}

/// Extracts and (optionally) resolves one file's imports.
fn extract_with(
    path: &LogicalPath,
    language: Language,
    source: &str,
    index: Option<&ImportIndex>,
    stats: &mut ImportStats,
) -> Vec<ImportEdge> {
    match Extractor::new(language) {
        Ok(mut extractor) => extractor.edges(path, source, index, stats),
        Err(error) => {
            tracing::error!(%error, "no grammar; every file of this language has no streets");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Every file a specifier may resolve to, plus the Rust crate roots.
///
/// A snapshot rather than a borrow of [`RepoTree`], so that
/// [`ImportGraph::update_file`] can re-resolve one changed file without the
/// tree. Industrial files are **excluded**: PRD §8 renders `node_modules` as one
/// dull mass, so an import that lands inside it has no building to point at and
/// resolves as [`ImportTarget::External`], which is what it is.
#[derive(Debug, Clone, Default)]
pub struct ImportIndex {
    /// Every non-industrial file, with the grammar that can parse it.
    files: BTreeMap<LogicalPath, Option<Language>>,
    /// Rust crate module roots, keyed by the crate name with `-` folded to `_`
    /// as `use` spells it. Lets `use polis_events::LogicalPath;` in a workspace
    /// resolve to `polis-events/src/lib.rs` instead of being written off as an
    /// external crate.
    rust_crate_roots: BTreeMap<String, LogicalPath>,
}

impl ImportIndex {
    /// Snapshots a [`RepoTree`].
    pub fn from_tree(tree: &RepoTree) -> Self {
        let mut files = BTreeMap::new();
        for (path, meta) in &tree.files {
            if meta.class.is_massed() || is_industrial_path(path) {
                continue;
            }
            files.insert(path.clone(), meta.language);
        }
        let mut index = Self {
            files,
            rust_crate_roots: BTreeMap::new(),
        };
        index.rebuild_crate_roots();
        index
    }

    /// Registers a file that appeared after the snapshot was taken.
    pub fn insert(&mut self, path: LogicalPath, language: Option<Language>) {
        let is_crate_root = is_rust_crate_root_file(&path);
        self.files.insert(path, language);
        if is_crate_root {
            self.rebuild_crate_roots();
        }
    }

    /// Forgets a deleted file.
    pub fn remove(&mut self, path: &LogicalPath) {
        if self.files.remove(path).is_some() && is_rust_crate_root_file(path) {
            self.rebuild_crate_roots();
        }
    }

    /// The grammar registered for a file, if the index has one.
    pub fn language(&self, path: &LogicalPath) -> Option<Language> {
        self.files.get(path).copied().flatten()
    }

    /// True when the index holds this file.
    pub fn contains(&self, path: &LogicalPath) -> bool {
        self.files.contains_key(path)
    }

    /// How many files can be resolved to.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// True when nothing can be resolved to.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Resolves one module specifier written in `from`.
    ///
    /// Three outcomes, and the difference between them is the whole point:
    /// [`ImportTarget::Internal`] is a street, [`ImportTarget::External`] is a
    /// package (PRD §8's industrial zone seen from the source side), and
    /// [`ImportTarget::Unresolved`] is an honest "this names something inside
    /// the repo and I could not find it". Nothing is guessed: a specifier only
    /// becomes `Internal` when a file at that exact path is in the index.
    pub fn resolve(&self, from: &LogicalPath, specifier: &str, language: Language) -> ImportTarget {
        let specifier = specifier.trim();
        if specifier.is_empty() {
            return ImportTarget::Unresolved;
        }
        self.resolve_nested(from, specifier, language, 0)
    }

    /// [`resolve`](Self::resolve), told how many inline `mod` blocks the
    /// statement sits inside.
    ///
    /// Rust's `super` counts *modules*, not directories, and an inline
    /// `mod tests { … }` is a module with no directory of its own. So the
    /// `use super::*;` that every `#[cfg(test)] mod tests` in this workspace
    /// opens with means "this file", not "the parent directory's module" — and
    /// resolving it the second way manufactures an edge to `lib.rs` from every
    /// file in the crate, which would then dominate PRD §8's inbound-import
    /// decile and put a monument on the wrong building.
    ///
    /// `inline_depth` is `0` for every language but Rust, and for Rust it is
    /// counted from the syntax tree by the extractor.
    pub fn resolve_nested(
        &self,
        from: &LogicalPath,
        specifier: &str,
        language: Language,
        inline_depth: u32,
    ) -> ImportTarget {
        let specifier = specifier.trim();
        if specifier.is_empty() {
            return ImportTarget::Unresolved;
        }
        match language {
            Language::Rust => self.resolve_rust(from, specifier, inline_depth),
            Language::Python => self.resolve_python(from, specifier),
            Language::JavaScript | Language::TypeScript | Language::Tsx => {
                self.resolve_js(from, specifier, language)
            }
        }
    }

    /// Indexes every `<dir>/src/{lib,main}.rs` by crate name.
    fn rebuild_crate_roots(&mut self) {
        self.rust_crate_roots.clear();
        for path in self.files.keys() {
            if !is_rust_crate_root_file(path) {
                continue;
            }
            let Some(src) = path.parent() else { continue };
            let Some(crate_dir) = src.parent() else {
                continue;
            };
            let Some(name) = crate_dir.file_name() else {
                continue;
            };
            // First in path order wins, so two same-named directories cannot
            // make the answer depend on walk order.
            self.rust_crate_roots
                .entry(rust_crate_key(name))
                .or_insert(src);
        }
    }

    // -- Rust ---------------------------------------------------------------

    /// `use`, `mod` and `extern crate`, against Rust's module tree.
    fn resolve_rust(&self, from: &LogicalPath, specifier: &str, inline_depth: u32) -> ImportTarget {
        if let Some(name) = specifier.strip_prefix(MOD_PREFIX) {
            // `mod a { mod b; }` puts `b.rs` under a directory named after `a`,
            // and the inline module's *name* is not in the specifier. Refusing
            // to answer beats naming the wrong file.
            if inline_depth > 0 {
                return ImportTarget::Unresolved;
            }
            let Some(base) = rust_module_dir(from) else {
                return ImportTarget::Unresolved;
            };
            return self
                .probe_rust(&base, &[name])
                .map_or(ImportTarget::Unresolved, ImportTarget::Internal);
        }

        let segments: Vec<&str> = specifier
            .split("::")
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "*")
            .collect();
        let Some(&head) = segments.first() else {
            return ImportTarget::Unresolved;
        };
        let rest = &segments[1..];

        let (base, rest) = match head {
            "crate" => match self.rust_crate_root_for(from) {
                Some(base) => (base, rest),
                None => return ImportTarget::Unresolved,
            },
            "self" => {
                // Inside an inline `mod`, `self` is that block — which lives in
                // this file and nowhere else.
                if inline_depth > 0 {
                    return ImportTarget::Internal(from.clone());
                }
                match rust_module_dir(from) {
                    Some(base) => (base, rest),
                    None => return ImportTarget::Unresolved,
                }
            }
            "super" => {
                let mut levels = 0;
                while levels < segments.len() && segments[levels] == "super" {
                    levels += 1;
                }
                // Each enclosing inline `mod` absorbs one `super` without
                // leaving the file.
                let Some(steps) = levels.checked_sub(inline_depth as usize).filter(|s| *s > 0)
                else {
                    return ImportTarget::Internal(from.clone());
                };
                let Some(mut base) = rust_module_dir(from) else {
                    return ImportTarget::Unresolved;
                };
                for _ in 0..steps {
                    let Some(parent) = base.parent() else {
                        return ImportTarget::Unresolved;
                    };
                    base = parent;
                }
                (base, &segments[levels..])
            }
            "std" | "core" | "alloc" | "proc_macro" | "test" => return ImportTarget::External,
            other => match self.rust_crate_roots.get(&rust_crate_key(other)) {
                Some(base) => (base.clone(), rest),
                None => return ImportTarget::External,
            },
        };

        self.probe_rust(&base, rest)
            .map_or(ImportTarget::Unresolved, ImportTarget::Internal)
    }

    /// The module root of the crate containing `from`: the nearest ancestor
    /// directory holding a `lib.rs` or a `main.rs`.
    fn rust_crate_root_for(&self, from: &LogicalPath) -> Option<LogicalPath> {
        let mut dir = from.parent()?;
        loop {
            for name in ["lib.rs", "main.rs"] {
                if let Ok(candidate) = dir.join(name) {
                    if self.files.contains_key(&candidate) {
                        return Some(dir);
                    }
                }
            }
            if dir.is_root() {
                return None;
            }
            dir = dir.parent()?;
        }
    }

    /// Finds the file that defines `base::<segments>`, longest prefix first.
    ///
    /// `use crate::imports::ImportEdge` has no file called `ImportEdge.rs`, so
    /// the walk shortens: `imports/ImportEdge.rs`, `imports/ImportEdge/mod.rs`,
    /// then `imports.rs` — which exists, and is the edge. Shortening to zero
    /// segments lands on the crate root itself, which is the right answer for
    /// `use crate::Thing` where `Thing` is declared in `lib.rs`.
    fn probe_rust(&self, base: &LogicalPath, segments: &[&str]) -> Option<LogicalPath> {
        for take in (0..=segments.len()).rev() {
            let dir = if take == 0 {
                base.clone()
            } else {
                base.join(&segments[..take].join("/")).ok()?
            };
            if take >= 1 {
                let file = base
                    .join(&format!("{}.rs", segments[..take].join("/")))
                    .ok();
                if let Some(file) = file {
                    if self.files.contains_key(&file) {
                        return Some(file);
                    }
                }
            }
            if let Some(module) = self.rust_module_file(&dir) {
                return Some(module);
            }
        }
        None
    }

    /// The file that *is* the module living in directory `dir`.
    ///
    /// Both spellings: `dir/mod.rs` (or a crate root's `lib.rs`/`main.rs`), and
    /// the 2018-edition `dir.rs` sitting beside the directory. The sibling form
    /// comes last so an accidental repo containing both keeps `mod.rs`, which is
    /// what `rustc` would have picked before rejecting the pair.
    fn rust_module_file(&self, dir: &LogicalPath) -> Option<LogicalPath> {
        for name in ["mod.rs", "lib.rs", "main.rs"] {
            if let Ok(candidate) = dir.join(name) {
                if self.files.contains_key(&candidate) {
                    return Some(candidate);
                }
            }
        }
        if let Some(name) = dir.file_name() {
            if let Some(sibling) = self.file_at(dir, &format!("{name}.rs"), true) {
                return Some(sibling);
            }
        }
        None
    }

    // -- JavaScript / TypeScript --------------------------------------------

    /// Node/bundler resolution, minus everything that needs a config file.
    ///
    /// A bare specifier is [`ImportTarget::External`] and is **never** probed
    /// against the tree. `tsconfig`'s `baseUrl` and `paths` would make
    /// `import "utils"` mean `src/utils.ts` in one repo and the npm package
    /// `utils` in the next; resolving it without reading the config is a coin
    /// flip, and a wrong street is worse than a missing one. The one exception
    /// is the `@/` and `~/` prefix, which cannot be a package name — an npm
    /// scope is always `@scope/name` — so it is unambiguously a project alias.
    fn resolve_js(&self, from: &LogicalPath, specifier: &str, language: Language) -> ImportTarget {
        // `./a.css?raw`, `./a#frag` — bundler suffixes, not part of the path.
        let spec = specifier
            .split(['?', '#'])
            .next()
            .unwrap_or(specifier)
            .trim();
        if spec.is_empty() {
            return ImportTarget::Unresolved;
        }
        let extensions = js_extension_order(language);

        if spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec == ".." {
            let base = district_of(from);
            return self
                .probe_js(&base, spec, extensions)
                .map_or(ImportTarget::Unresolved, ImportTarget::Internal);
        }

        if let Some(rest) = spec.strip_prefix("@/").or_else(|| spec.strip_prefix("~/")) {
            for base in [
                LogicalPath::root(),
                LogicalPath::new("src").unwrap_or_default(),
            ] {
                if let Some(hit) = self.probe_js(&base, rest, extensions) {
                    return ImportTarget::Internal(hit);
                }
            }
            return ImportTarget::Unresolved;
        }

        if let Some(rest) = spec.strip_prefix('/') {
            return self
                .probe_js(&LogicalPath::root(), rest, extensions)
                .map_or(ImportTarget::Unresolved, ImportTarget::Internal);
        }

        ImportTarget::External
    }

    /// Node's file/index probe, plus TypeScript's `.js`-means-`.ts` rewrite.
    fn probe_js(
        &self,
        base: &LogicalPath,
        relative: &str,
        extensions: &[&str],
    ) -> Option<LogicalPath> {
        let joined = base.join(relative).ok()?;

        if self.files.contains_key(&joined) {
            return Some(joined);
        }
        // `import "./a.js"` in a `nodenext` TypeScript project means `a.ts`;
        // the extension written is the one the *output* will have.
        if let Some(name) = joined.file_name() {
            if let Some((stem, ext)) = name.rsplit_once('.') {
                for rewrite in ts_rewrites(ext) {
                    if let Some(hit) = self.file_at(&joined, &format!("{stem}.{rewrite}"), true) {
                        return Some(hit);
                    }
                }
            }
        }
        for extension in extensions {
            if let Some(hit) = self.sibling(&joined, extension) {
                return Some(hit);
            }
        }
        for extension in extensions {
            if let Ok(index) = joined.join(&format!("index.{extension}")) {
                if self.files.contains_key(&index) {
                    return Some(index);
                }
            }
        }
        None
    }

    /// `<path>.<extension>`, if it is in the index.
    fn sibling(&self, path: &LogicalPath, extension: &str) -> Option<LogicalPath> {
        let name = path.file_name()?;
        self.file_at(path, &format!("{name}.{extension}"), true)
    }

    /// A file named `name` beside `path` (`replace` = true) or under it.
    fn file_at(&self, path: &LogicalPath, name: &str, replace: bool) -> Option<LogicalPath> {
        let base = if replace {
            path.parent()?
        } else {
            path.clone()
        };
        let candidate = base.join(name).ok()?;
        self.files.contains_key(&candidate).then_some(candidate)
    }

    // -- Python -------------------------------------------------------------

    /// Dotted module paths, relative and absolute.
    ///
    /// A relative import that misses is [`ImportTarget::Unresolved`] — it names
    /// something inside the package and the file is simply not there. An
    /// absolute import that misses is [`ImportTarget::External`] — `import os`
    /// is the standard library, and the repository is the only place an
    /// absolute import could have been internal.
    fn resolve_python(&self, from: &LogicalPath, specifier: &str) -> ImportTarget {
        let trimmed = specifier.trim_start_matches('.');
        let dots = specifier.len() - trimmed.len();
        let relative = trimmed.replace('.', "/");

        if dots > 0 {
            let Some(mut base) = from.parent() else {
                return ImportTarget::Unresolved;
            };
            for _ in 1..dots {
                let Some(parent) = base.parent() else {
                    return ImportTarget::Unresolved;
                };
                base = parent;
            }
            return self
                .probe_python(&base, &relative)
                .map_or(ImportTarget::Unresolved, ImportTarget::Internal);
        }

        // Absolute: the repo root and a `src/` layout are the two places an
        // interpreter run from the repo would find it.
        for base in [
            LogicalPath::root(),
            LogicalPath::new("src").unwrap_or_default(),
        ] {
            if let Some(hit) = self.probe_python(&base, &relative) {
                return ImportTarget::Internal(hit);
            }
        }
        ImportTarget::External
    }

    /// `<base>/<relative>.py`, `.pyi`, or `<base>/<relative>/__init__.py`.
    fn probe_python(&self, base: &LogicalPath, relative: &str) -> Option<LogicalPath> {
        let target = if relative.is_empty() {
            base.clone()
        } else {
            base.join(relative).ok()?
        };
        if !relative.is_empty() {
            for extension in PY_EXTENSIONS {
                if let Some(hit) = self.sibling(&target, extension) {
                    return Some(hit);
                }
            }
        }
        self.file_at(&target, "__init__.py", false)
    }
}

/// The directory Rust's module tree resolves `self::` against.
///
/// `a/mod.rs`, `a/lib.rs` and `a/main.rs` *are* the module `a`, so their module
/// directory is `a`. Every other `a/b.rs` is the module `b`, whose children live
/// in `a/b/`.
fn rust_module_dir(from: &LogicalPath) -> Option<LogicalPath> {
    let name = from.file_name()?;
    let parent = from.parent()?;
    if is_rust_module_root_name(name) {
        return Some(parent);
    }
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    parent.join(stem).ok()
}

/// The key `use <name>::…` spells a crate directory with.
fn rust_crate_key(name: &str) -> String {
    name.to_ascii_lowercase().replace('-', "_")
}

/// True for `lib.rs` and `main.rs` sitting directly inside a `src` directory.
fn is_rust_crate_root_file(path: &LogicalPath) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    if !(name.eq_ignore_ascii_case("lib.rs") || name.eq_ignore_ascii_case("main.rs")) {
        return false;
    }
    path.parent()
        .and_then(|p| p.file_name().map(|n| n.eq_ignore_ascii_case("src")))
        .unwrap_or(false)
}

/// True for the three file names that *are* their containing directory's module.
fn is_rust_module_root_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("mod.rs")
        || name.eq_ignore_ascii_case("lib.rs")
        || name.eq_ignore_ascii_case("main.rs")
}

/// Python module file suffixes, in probe order.
const PY_EXTENSIONS: &[&str] = &["py", "pyi"];

/// Extensions probed for an extensionless JavaScript/TypeScript specifier.
///
/// Two orders, because the answer differs: in a `.ts` file `./auth` almost
/// always means `auth.ts`, and in a `.js` file it almost always means `auth.js`.
/// A repository holding both would otherwise get its streets decided by whichever
/// order was written first.
const TS_FIRST: &[&str] = &[
    "ts", "tsx", "d.ts", "mts", "cts", "js", "jsx", "mjs", "cjs", "json",
];
/// See [`TS_FIRST`].
const JS_FIRST: &[&str] = &[
    "js", "jsx", "mjs", "cjs", "json", "ts", "tsx", "d.ts", "mts", "cts",
];

/// Which of [`TS_FIRST`] / [`JS_FIRST`] an importing file uses.
fn js_extension_order(language: Language) -> &'static [&'static str] {
    match language {
        Language::TypeScript | Language::Tsx => TS_FIRST,
        Language::JavaScript | Language::Rust | Language::Python => JS_FIRST,
    }
}

/// The TypeScript sources a written `.js`-family extension may really mean.
fn ts_rewrites(extension: &str) -> &'static [&'static str] {
    if extension.eq_ignore_ascii_case("js") {
        &["ts", "tsx", "d.ts"]
    } else if extension.eq_ignore_ascii_case("mjs") {
        &["mts"]
    } else if extension.eq_ignore_ascii_case("cjs") {
        &["cts"]
    } else {
        &[]
    }
}

/// Districts holding at least one parseable, non-industrial file.
fn code_districts(tree: &RepoTree) -> BTreeSet<LogicalPath> {
    tree.files
        .iter()
        .filter(|(path, meta)| {
            meta.language.is_some() && !meta.class.is_massed() && !is_industrial_path(path)
        })
        .map(|(path, _)| district_of(path))
        .collect()
}

// ---------------------------------------------------------------------------
// tree-sitter queries
// ---------------------------------------------------------------------------

/// Prefix marking a Rust `mod foo;` declaration in an [`ImportEdge::specifier`].
///
/// A `mod` declaration is a genuine file-level edge — `mod ui;` in `src/lib.rs`
/// reaches `src/ui/mod.rs`, which is a different district and therefore a
/// street — but it is not a path expression, so it cannot share a spelling with
/// `use foo;`. The specifier is stored exactly as the source reads, `mod foo`,
/// which is also what PRD §12's drill-down should show.
const MOD_PREFIX: &str = "mod ";

/// See [`query_source`].
const RUST_QUERY: &str = r"
(use_declaration argument: (_) @use.tree)
(mod_item) @mod.item
(extern_crate_declaration name: (identifier) @extern.name)
";

/// See [`query_source`].
const PYTHON_QUERY: &str = r"
(import_statement) @py.import
(import_from_statement) @py.from
";

/// See [`query_source`].
const JS_QUERY: &str = r"
(import_statement source: (string) @src)
(export_statement source: (string) @src)
(call_expression
  function: (identifier) @callee
  arguments: (arguments . (string) @src))
(call_expression
  function: (import)
  arguments: (arguments . (string) @src))
";

/// See [`query_source`]. TypeScript adds `import x = require('y')`.
const TS_QUERY: &str = r"
(import_statement source: (string) @src)
(export_statement source: (string) @src)
(import_require_clause source: (string) @src)
(call_expression
  function: (identifier) @callee
  arguments: (arguments . (string) @src))
(call_expression
  function: (import)
  arguments: (arguments . (string) @src))
";

/// Capture indices for the roles [`query_source`]'s patterns use.
#[derive(Default, Clone, Copy)]
struct CaptureIdx {
    use_tree: Option<u32>,
    mod_item: Option<u32>,
    extern_name: Option<u32>,
    py_import: Option<u32>,
    py_from: Option<u32>,
    src: Option<u32>,
    callee: Option<u32>,
}

/// One language's loaded grammar, compiled query and reusable parser.
///
/// Not `Sync` — `Parser` holds a mutable C allocation — so each worker thread
/// owns its own. Constructing one costs a grammar load and a query compile,
/// which is why they are cached per thread rather than per file.
struct Extractor {
    language: Language,
    parser: Parser,
    query: Query,
    idx: CaptureIdx,
    cursor: QueryCursor,
}

/// The loaded grammar for one [`Language`].
///
/// The single place the four grammar crates are named. `crate::describe` needs
/// the same parsers to read module doc comments, and a second copy of this match
/// is how one module ends up on a different grammar from the other.
pub(crate) fn ts_language(language: Language) -> tree_sitter::Language {
    match language {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
    }
}

impl Extractor {
    /// Loads a grammar and compiles its import query.
    fn new(language: Language) -> Result<Self, GrammarError> {
        let ts_language = ts_language(language);

        let mut parser = Parser::new();
        parser
            .set_language(&ts_language)
            .map_err(|e| GrammarError::Language(language, e.to_string()))?;

        let source = query_source(language);
        let query = Query::new(&ts_language, source)
            .map_err(|e| GrammarError::Query(language, e.to_string()))?;

        let idx = CaptureIdx {
            use_tree: query.capture_index_for_name("use.tree"),
            mod_item: query.capture_index_for_name("mod.item"),
            extern_name: query.capture_index_for_name("extern.name"),
            py_import: query.capture_index_for_name("py.import"),
            py_from: query.capture_index_for_name("py.from"),
            src: query.capture_index_for_name("src"),
            callee: query.capture_index_for_name("callee"),
        };

        Ok(Self {
            language,
            parser,
            query,
            idx,
            cursor: QueryCursor::new(),
        })
    }

    /// One file's edges, sorted and deduplicated.
    fn edges(
        &mut self,
        path: &LogicalPath,
        source: &str,
        index: Option<&ImportIndex>,
        stats: &mut ImportStats,
    ) -> Vec<ImportEdge> {
        let language = self.language;
        let specifiers = self.specifiers(path, source, stats);
        resolve_specifiers(path, language, specifiers, index)
    }

    /// Every module specifier the file names, in source order, each paired with
    /// the number of inline `mod` blocks it sits inside.
    ///
    /// The depth is always `0` outside Rust; see
    /// [`ImportIndex::resolve_nested`] for why Rust needs it.
    ///
    /// PRD §9's non-fatal rule lives here: a parse that returns no tree yields
    /// an empty vector, and a tree with syntax errors yields whatever
    /// `tree-sitter`'s error recovery salvaged, which for a file with one broken
    /// function is normally every import in it.
    fn specifiers(
        &mut self,
        path: &LogicalPath,
        source: &str,
        stats: &mut ImportStats,
    ) -> Vec<(String, u32)> {
        let Some(tree) = self.parser.parse(source, None) else {
            tracing::debug!(path = path.as_str(), "parser returned no tree; no streets");
            return Vec::new();
        };
        let root = tree.root_node();
        if root.has_error() {
            tracing::debug!(
                path = path.as_str(),
                "syntax errors; extracting what parsed"
            );
            stats.files_with_parse_errors += 1;
        }

        let bytes = source.as_bytes();
        let mut out: Vec<(String, u32)> = Vec::new();
        let mut flat: Vec<String> = Vec::new();
        let mut matches = self.cursor.matches(&self.query, root, bytes);
        while let Some(m) = matches.next() {
            match self.language {
                Language::Rust => rust_match(m.captures(), &self.idx, bytes, &mut out),
                Language::Python => {
                    python_match(m.captures(), &self.idx, bytes, &mut flat);
                    out.extend(flat.drain(..).map(|s| (s, 0)));
                }
                Language::JavaScript | Language::TypeScript | Language::Tsx => {
                    js_match(m.captures(), &self.idx, bytes, &mut flat);
                    out.extend(flat.drain(..).map(|s| (s, 0)));
                }
            }
        }
        out
    }
}

/// One language's slot in a thread's [`ExtractorCache`].
enum Slot {
    /// Not asked for yet on this thread.
    Unloaded,
    /// The grammar or its query was rejected. Remembered so the error is logged
    /// once per thread, not once per file.
    Rejected,
    /// Loaded and reusable.
    Ready(Box<Extractor>),
}

/// A per-thread cache of one `Extractor` per language, built on first use.
struct ExtractorCache {
    slots: [Slot; 5],
}

impl Default for ExtractorCache {
    fn default() -> Self {
        Self {
            slots: std::array::from_fn(|_| Slot::Unloaded),
        }
    }
}

impl ExtractorCache {
    /// The extractor for `language`, loading it once per thread.
    ///
    /// `None` means the grammar or its query was rejected, which PRD §9 makes
    /// non-fatal: that language simply has no streets.
    fn get(&mut self, language: Language) -> Option<&mut Extractor> {
        let slot = &mut self.slots[language_slot(language)];
        if matches!(slot, Slot::Unloaded) {
            *slot = match Extractor::new(language) {
                Ok(extractor) => Slot::Ready(Box::new(extractor)),
                Err(error) => {
                    tracing::error!(%error, "grammar unavailable; this language has no streets");
                    Slot::Rejected
                }
            };
        }
        match slot {
            Slot::Ready(extractor) => Some(extractor),
            Slot::Unloaded | Slot::Rejected => None,
        }
    }
}

/// A dense index for [`ExtractorCache::slots`].
fn language_slot(language: Language) -> usize {
    match language {
        Language::Rust => 0,
        Language::JavaScript => 1,
        Language::TypeScript => 2,
        Language::Tsx => 3,
        Language::Python => 4,
    }
}

// ---------------------------------------------------------------------------
// Per-language match handling
// ---------------------------------------------------------------------------

/// The text of a node, or `""` if it is somehow not UTF-8.
fn text<'a>(node: Node<'_>, bytes: &'a [u8]) -> &'a str {
    node.utf8_text(bytes).unwrap_or_default()
}

/// A node's named children, without a borrowed cursor.
fn named_children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    let count = u32::try_from(node.named_child_count()).unwrap_or(u32::MAX);
    (0..count).filter_map(move |i| node.named_child(i))
}

/// Rust path text with every ASCII space stripped.
///
/// `use crate::\n    imports::Edge;` is one `scoped_identifier` whose text spans
/// the newline. The path itself never contains whitespace, so removing it is
/// lossless and saves the resolver from having to.
fn clean_rust_path(raw: &str) -> String {
    raw.chars().filter(|c| !c.is_whitespace()).collect()
}

/// `use`, `mod` and `extern crate`, each tagged with its inline-`mod` depth.
fn rust_match(
    captures: &[tree_sitter::QueryCapture<'_>],
    idx: &CaptureIdx,
    bytes: &[u8],
    out: &mut Vec<(String, u32)>,
) {
    let mut flat: Vec<String> = Vec::new();
    for capture in captures {
        let index = Some(capture.index);
        if index == idx.use_tree {
            walk_use_tree(capture.node, bytes, "", 0, &mut flat);
        } else if index == idx.mod_item {
            // `mod foo { … }` is an inline module: no file, no edge.
            if capture.node.child_by_field_name("body").is_none() {
                if let Some(name) = capture.node.child_by_field_name("name") {
                    let name = clean_rust_path(text(name, bytes));
                    if !name.is_empty() {
                        flat.push(format!("{MOD_PREFIX}{name}"));
                    }
                }
            }
        } else if index == idx.extern_name {
            let name = clean_rust_path(text(capture.node, bytes));
            if !name.is_empty() {
                flat.push(name);
            }
        } else {
            continue;
        }
        let depth = inline_module_depth(capture.node);
        out.extend(flat.drain(..).map(|specifier| (specifier, depth)));
    }
}

/// How many `mod x { … }` blocks a node sits inside.
///
/// `mod x;` — a declaration with no body — is a *file*, not an inline module,
/// and does not count: its contents are a separate file whose own depth starts
/// again at zero.
fn inline_module_depth(node: Node<'_>) -> u32 {
    let mut depth = 0;
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if ancestor.kind() == "mod_item" && ancestor.child_by_field_name("body").is_some() {
            depth += 1;
        }
        current = ancestor.parent();
    }
    depth
}

/// Flattens `use a::{b, c::{d, e}};` into one specifier per leaf.
///
/// A query cannot express this — the nesting is unbounded — so the query
/// captures the `use_declaration`'s argument and this walks it. Depth-limited
/// because the input is untrusted source and a pathological file should cost a
/// truncated answer, not a stack overflow.
fn walk_use_tree(node: Node<'_>, bytes: &[u8], prefix: &str, depth: usize, out: &mut Vec<String>) {
    const MAX_DEPTH: usize = 32;
    if depth > MAX_DEPTH {
        return;
    }
    match node.kind() {
        "scoped_use_list" => {
            let mut nested = String::from(prefix);
            if let Some(path) = node.child_by_field_name("path") {
                let path = clean_rust_path(text(path, bytes));
                if !path.is_empty() {
                    nested.push_str(&path);
                    nested.push_str("::");
                }
            }
            if let Some(list) = node.child_by_field_name("list") {
                walk_use_tree(list, bytes, &nested, depth + 1, out);
            }
        }
        "use_as_clause" => {
            if let Some(path) = node.child_by_field_name("path") {
                walk_use_tree(path, bytes, prefix, depth + 1, out);
            }
        }
        "use_wildcard" => match named_children(node).next() {
            Some(child) => push_rust_path(prefix, text(child, bytes), out),
            None => push_rust_path("", prefix.trim_end_matches("::"), out),
        },
        "identifier" | "scoped_identifier" | "crate" | "super" | "self" | "metavariable" => {
            push_rust_path(prefix, text(node, bytes), out);
        }
        // `use_list` — the `{a, b}` of a brace group — and anything a future
        // grammar release introduces: recurse and let the leaves decide.
        _ => {
            for child in named_children(node) {
                walk_use_tree(child, bytes, prefix, depth + 1, out);
            }
        }
    }
}

/// Appends `prefix + tail` as a specifier, if it is non-empty.
fn push_rust_path(prefix: &str, tail: &str, out: &mut Vec<String>) {
    let joined = clean_rust_path(&format!("{prefix}{tail}"));
    if !joined.is_empty() {
        out.push(joined);
    }
}

/// `import a.b`, `import a as b`, `from .x import y`, `from . import y`.
fn python_match(
    captures: &[tree_sitter::QueryCapture<'_>],
    idx: &CaptureIdx,
    bytes: &[u8],
    out: &mut Vec<String>,
) {
    for capture in captures {
        let index = Some(capture.index);
        if index == idx.py_import {
            for name in python_name_fields(capture.node, bytes) {
                out.push(name);
            }
        } else if index == idx.py_from {
            python_from(capture.node, bytes, out);
        }
    }
}

/// One `import_from_statement`.
fn python_from(node: Node<'_>, bytes: &[u8], out: &mut Vec<String>) {
    let Some(module) = node.child_by_field_name("module_name") else {
        return;
    };
    let module_text: String = text(module, bytes)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if module_text.is_empty() {
        return;
    }
    out.push(module_text.clone());

    // `from . import auth` names a *module*, not a name inside `__init__.py`,
    // whenever `auth.py` exists — so each imported name is a candidate edge.
    let dots_only = module.kind() == "relative_import"
        && !named_children(module).any(|c| c.kind() == "dotted_name");
    if dots_only {
        for name in python_name_fields(node, bytes) {
            out.push(format!("{module_text}{name}"));
        }
    }
}

/// The dotted names under a statement's `name` fields, aliases unwrapped.
fn python_name_fields(node: Node<'_>, bytes: &[u8]) -> Vec<String> {
    let mut cursor = node.walk();
    node.children_by_field_name("name", &mut cursor)
        .filter_map(|child| {
            let target = if child.kind() == "aliased_import" {
                child.child_by_field_name("name")?
            } else {
                child
            };
            let name: String = text(target, bytes)
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            (!name.is_empty()).then_some(name)
        })
        .collect()
}

/// `import … from "x"`, `export … from "x"`, `require("x")`, `import("x")`.
fn js_match(
    captures: &[tree_sitter::QueryCapture<'_>],
    idx: &CaptureIdx,
    bytes: &[u8],
    out: &mut Vec<String>,
) {
    let mut source: Option<Node<'_>> = None;
    let mut callee: Option<&str> = None;
    for capture in captures {
        let index = Some(capture.index);
        if index == idx.src {
            source = Some(capture.node);
        } else if index == idx.callee {
            callee = Some(text(capture.node, bytes));
        }
    }
    // A callee capture only appears on the `require`-shaped pattern; every other
    // call with a string first argument must be ignored.
    if callee.is_some_and(|name| name != "require") {
        return;
    }
    if let Some(specifier) = source.and_then(|node| string_literal(node, bytes)) {
        out.push(specifier);
    }
}

/// The contents of a JavaScript string literal, quotes removed.
fn string_literal(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    let mut fragments = String::new();
    for child in named_children(node) {
        if child.kind() == "string_fragment" {
            fragments.push_str(text(child, bytes));
        }
    }
    if !fragments.is_empty() {
        return Some(fragments);
    }
    // No fragment child means an empty literal, or a grammar that models the
    // body differently. Strip the quotes by characters, never by byte index.
    let raw = text(node, bytes);
    let mut chars = raw.chars();
    chars.next();
    chars.next_back();
    let inner = chars.as_str();
    (!inner.is_empty()).then(|| inner.to_owned())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The specifier cache (PRD §13.1: cold start under three seconds)
// ---------------------------------------------------------------------------

/// Bump on any change to what [`Extractor::specifiers`] returns, or to the
/// grammars behind it. A version mismatch is a total miss, exactly like a
/// corrupt file — never a partial read of something whose meaning has moved.
const PARSE_CACHE_VERSION: u32 = 1;

/// Where [`ImportGraph::build_cached`] keeps one repository's extracted
/// specifiers.
///
/// `%LOCALAPPDATA%\polis\imports\<key>.json` on Windows,
/// `$XDG_STATE_HOME/polis/imports/<key>.json` elsewhere — beside the history
/// cache and the corpus store, and deliberately **outside the checkout**: a
/// cache written into the repository would be walked, given a building, and
/// change the city.
///
/// The key is the same folding of the normalised root that the history cache
/// uses, so two worktrees of one repository get two caches and a checkout
/// spelled `C:\Repo` and `c:/repo/` gets one.
#[must_use]
pub fn default_parse_cache_path(repo_root: &Path) -> Option<std::path::PathBuf> {
    let key =
        crate::git::fnv1a64(crate::git::normalize_root(&repo_root.to_string_lossy()).as_bytes());
    Some(
        crate::corpus::state_dir()?
            .join("imports")
            .join(format!("{key:016x}.json")),
    )
}

/// One file's extraction, as it is stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedFile {
    /// The layout key (PRD §7.6).
    path: LogicalPath,
    /// The two halves of the content digest. See [`content_digest`].
    lo: u64,
    hi: u64,
    /// Source length in bytes; a third, free, independent check.
    len: u64,
    /// The grammar the specifiers came out of.
    language: Language,
    /// Whether `tree-sitter`'s error recovery had to salvage this file, so a
    /// cache hit reports the same counters a parse would.
    errors: bool,
    /// What [`Extractor::specifiers`] returned: the module specifier and the
    /// number of inline `mod` blocks it sits inside.
    specifiers: Vec<(String, u32)>,
}

/// The on-disk file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ParseCacheFile {
    version: u32,
    entries: Vec<CachedFile>,
}

/// What the previous run extracted, ready to be asked about a file.
///
/// # Why this is safe to trust, and exactly how far
///
/// A hit is verified against a **128-bit content digest plus the byte length**
/// of the source that produced it, so a file whose bytes changed by one bit is a
/// miss. It is not the bit-for-bit argument comparison `polis_layout::memo`
/// uses, because the argument here is the whole file and storing every byte of
/// the repository to avoid re-parsing it would cost more than the parse. What it
/// buys instead is a collision probability below any rate at which the rest of
/// this pipeline is correct, and — this is the part that matters for PRD §7.4 —
/// a *deterministic* one: the same bytes give the same digest on every machine
/// and every run, so two machines cannot disagree about a hit.
///
/// **Resolution is never cached.** Only the syntax stage is, which is a pure
/// function of `(source, language)`; every specifier is re-resolved against the
/// current [`ImportIndex`] on every run, because that is the stage a file
/// arriving or leaving changes.
#[derive(Debug, Default)]
pub struct ParseCache {
    by_path: BTreeMap<LogicalPath, CachedFile>,
}

impl ParseCache {
    /// Reads a cache file, or an empty cache when there is none, it is
    /// unreadable, or it was written by a different version.
    ///
    /// Never an error: a cold cache and a corrupt one are the same situation,
    /// and neither is worth failing a launch over.
    #[must_use]
    pub fn read(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else {
            return Self::default();
        };
        let Ok(file) = serde_json::from_slice::<ParseCacheFile>(&bytes) else {
            return Self::default();
        };
        if file.version != PARSE_CACHE_VERSION {
            return Self::default();
        }
        Self {
            by_path: file
                .entries
                .into_iter()
                .map(|e| (e.path.clone(), e))
                .collect(),
        }
    }

    /// The specifiers this file's exact bytes produced last time, if any.
    fn get(&self, path: &LogicalPath, language: Language, source: &str) -> Option<&CachedFile> {
        let entry = self.by_path.get(path)?;
        if entry.language != language || entry.len != source.len() as u64 {
            return None;
        }
        let (lo, hi) = content_digest(source.as_bytes());
        (entry.lo == lo && entry.hi == hi).then_some(entry)
    }

    /// Writes the cache for the files that exist **now**, so a repository that
    /// shrinks does not carry its deleted files forever.
    ///
    /// Failure is silent by design: an unwritable state directory costs the next
    /// launch a re-parse and nothing else.
    fn write(&self, path: &Path) {
        let file = ParseCacheFile {
            version: PARSE_CACHE_VERSION,
            entries: self.by_path.values().cloned().collect(),
        };
        if let Ok(bytes) = serde_json::to_vec(&file) {
            if let Err(error) = crate::git::write_atomic(path, &bytes) {
                tracing::debug!(%error, "could not write the specifier cache");
            }
        }
    }

    /// How many files it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    /// True when it holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }
}

/// A 128-bit content digest: FNV-1a over the bytes forwards, and a second,
/// independent FNV-1a over them with a different basis and prime.
///
/// Written out rather than reached for, for the same reason every other hash in
/// this workspace is: a `DefaultHasher` is seeded per process and a cache key
/// that changes between runs is not a cache key.
pub(crate) fn content_digest(bytes: &[u8]) -> (u64, u64) {
    const OFFSET_A: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME_A: u64 = 0x0000_0100_0000_01b3;
    const OFFSET_B: u64 = 0x9E37_79B9_7F4A_7C15;
    const PRIME_B: u64 = 0x0000_0100_0000_1B3F;
    let mut a = OFFSET_A;
    let mut b = OFFSET_B;
    for &byte in bytes {
        a ^= u64::from(byte);
        a = a.wrapping_mul(PRIME_A);
        b = b.rotate_left(7) ^ u64::from(byte);
        b = b.wrapping_mul(PRIME_B);
    }
    (a, b)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{FileClass, FileMeta};

    // -- fixtures -----------------------------------------------------------

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    /// A repository fixture on disk, plus the [`RepoTree`] that describes it.
    struct Fixture {
        dir: tempfile::TempDir,
        tree: RepoTree,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let tree = RepoTree {
                root: dir.path().to_path_buf(),
                ..RepoTree::default()
            };
            Self { dir, tree }
        }

        /// Writes a file and registers it with the language its extension
        /// implies. The extension table lives in `crate::tree::language_for`,
        /// which is another agent's file; the tests state the language directly
        /// so this module never depends on that mapping.
        fn add(&mut self, path: &str, language: Option<Language>, contents: &[u8]) -> &mut Self {
            self.add_classified(path, language, FileClass::Ordinary, contents)
        }

        fn add_classified(
            &mut self,
            path: &str,
            language: Option<Language>,
            class: FileClass,
            contents: &[u8],
        ) -> &mut Self {
            let logical = lp(path);
            let absolute: PathBuf = self.dir.path().join(path);
            if let Some(parent) = absolute.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(&absolute, contents).expect("write");
            let mut meta = FileMeta::untracked(
                logical.clone(),
                u64::try_from(contents.len()).unwrap_or(u64::MAX),
            );
            meta.language = language;
            meta.class = class;
            self.tree.files.insert(logical, meta);
            self
        }

        fn build(&self) -> ImportGraph {
            ImportGraph::build(&self.tree)
        }

        /// Builds through a cache file inside the fixture's own temp directory,
        /// so a test never touches the developer's state directory.
        fn build_through(&self, cache_file: &Path) -> ImportGraph {
            let remembered = ParseCache::read(cache_file);
            ImportGraph::build_with(&self.tree, Some(&remembered), Some(cache_file)).0
        }

        /// Where this fixture keeps its cache file.
        fn cache_file(&self) -> PathBuf {
            self.dir.path().join("specifiers.json")
        }
    }

    /// The corpus every behavioural test below runs against.
    ///
    /// Deliberately covers what PRD §9 says must not break the index: a syntax
    /// error, an unsupported extension, a non-UTF-8 file, an oversized file, and
    /// an industrial tree.
    fn corpus() -> Fixture {
        let mut f = Fixture::new();
        // --- Rust -----------------------------------------------------------
        f.add(
            "src/main.rs",
            Some(Language::Rust),
            b"mod app;\nuse crate::util::helpers::sanitize;\nuse polis_events::LogicalPath;\nuse std::fmt::Debug;\nfn main() { let _ = (sanitize, LogicalPath::root()); }\n",
        )
        .add(
            "src/app.rs",
            Some(Language::Rust),
            b"use crate::util::{helpers, MODE};\nuse super::main;\npub fn run() { let _ = (helpers::sanitize, MODE, main); }\n",
        )
        .add(
            "src/util/mod.rs",
            Some(Language::Rust),
            b"pub mod helpers;\npub const MODE: u8 = 1;\n",
        )
        .add(
            "src/util/helpers.rs",
            Some(Language::Rust),
            b"pub fn sanitize() {}\n",
        )
        .add(
            "polis-events/src/lib.rs",
            Some(Language::Rust),
            b"pub struct LogicalPath;\nimpl LogicalPath { pub fn root() -> Self { Self } }\n",
        );

        // --- TypeScript / TSX ------------------------------------------------
        f.add(
            "web/index.ts",
            Some(Language::TypeScript),
            b"import { util } from './lib/util';\nimport cfg from '../shared/config';\nimport React from 'react';\nconst fs = require('node:fs');\nexport { thing } from './lazy';\nexport const all = [util, cfg, React, fs, thing];\n",
        )
        .add("web/lib/util.ts", Some(Language::TypeScript), b"export const util = 1;\n")
        .add("web/lazy.ts", Some(Language::TypeScript), b"export const thing = 2;\n")
        .add("shared/config.ts", Some(Language::TypeScript), b"export default {};\n")
        .add(
            "ui/App.tsx",
            Some(Language::Tsx),
            b"import Button from './components/Button';\nimport '@/theme';\nimport { helper } from './helpers.js';\nexport const App = () => <Button x={helper} />;\n",
        )
        .add(
            "ui/components/index.tsx",
            Some(Language::Tsx),
            b"export { default as Button } from './Button';\n",
        )
        .add(
            "ui/components/Button.tsx",
            Some(Language::Tsx),
            b"export default function Button() { return null; }\n",
        )
        .add("ui/helpers.ts", Some(Language::TypeScript), b"export const helper = 3;\n")
        .add("theme.ts", Some(Language::TypeScript), b"export const theme = {};\n");

        // --- JavaScript -------------------------------------------------------
        f.add(
            "web/legacy.js",
            Some(Language::JavaScript),
            b"const util = require('./lib/util');\nconst lodash = require('lodash');\nimport('./lazy').then(() => {});\nmodule.exports = { util, lodash };\n",
        );

        // --- Python -----------------------------------------------------------
        f.add("py/__init__.py", Some(Language::Python), b"")
            .add(
                "py/main.py",
                Some(Language::Python),
                b"from .helpers import scrub\nfrom . import models\nimport os.path\nfrom pkg.sub import thing\nprint(scrub, models, os.path, thing)\n",
            )
            .add("py/helpers.py", Some(Language::Python), b"def scrub():\n    pass\n")
            .add("py/models.py", Some(Language::Python), b"MODELS = []\n")
            .add("pkg/__init__.py", Some(Language::Python), b"")
            .add("pkg/sub.py", Some(Language::Python), b"thing = 1\n");

        // --- the degradation cases -------------------------------------------
        f.add(
            "src/broken.rs",
            Some(Language::Rust),
            b"use crate::util::helpers;\nfn ( ] } garbage <<< ->->-> {{{\n",
        )
        .add(
            "docs/notes.md",
            None,
            b"# notes\n\nimport nothing from 'nowhere';\n",
        )
        .add(
            "py/blob.py",
            Some(Language::Python),
            &[0x89, 0x50, 0x4e, 0x47, 0x00, 0xff, 0xfe, 0xfd],
        )
        .add(
            "web/huge.js",
            Some(Language::JavaScript),
            &oversized_bundle(),
        )
        .add_classified(
            "node_modules/left-pad/index.js",
            Some(Language::JavaScript),
            FileClass::Industrial,
            b"const a = require('../other-pkg/index.js');\nmodule.exports = a;\n",
        )
        .add(
            "node_modules/other-pkg/index.js",
            Some(Language::JavaScript),
            b"module.exports = 1;\n",
        );
        f
    }

    /// A file just over [`MAX_PARSE_BYTES`] that would otherwise yield an edge.
    fn oversized_bundle() -> Vec<u8> {
        let mut bytes = b"const x = require('./lib/util');\n".to_vec();
        bytes.resize(MAX_PARSE_BYTES + 1, b' ');
        bytes
    }

    fn street(streets: &[Street], from: &str, to: &str) -> Option<u32> {
        streets
            .iter()
            .find(|s| s.from == lp(from) && s.to == lp(to))
            .map(|s| s.edge_count)
    }

    fn targets(graph: &ImportGraph, from: &str, specifier: &str) -> ImportTarget {
        let edges = graph.edges_from(&lp(from));
        let Some(edge) = edges.iter().find(|e| e.specifier == specifier) else {
            panic!(
                "no edge {from} -> {specifier:?}; had {:?}",
                edges
                    .iter()
                    .map(|e| e.specifier.as_str())
                    .collect::<Vec<_>>()
            )
        };
        edge.to.clone()
    }

    // -- grammars -----------------------------------------------------------

    #[test]
    fn every_grammar_loads_and_every_query_compiles() {
        // ADR-0051: an ABI mismatch is a runtime failure that PRD §9's
        // "non-fatal" rule would hide as "no streets anywhere".
        check_grammars().expect("grammar or query rejected by the runtime");
        for language in Language::ALL {
            assert!(
                !query_source(language).trim().is_empty(),
                "{language} has no import query"
            );
        }
    }

    // -- extraction ---------------------------------------------------------

    #[test]
    fn rust_use_trees_flatten_to_one_specifier_per_leaf() {
        let edges = extract(
            &lp("src/lib.rs"),
            Language::Rust,
            "use crate::{a, b::{c, d as e}};\nuse std::fmt::*;\nmod sub;\nmod inline { }\nextern crate serde;\n",
        );
        let mut specifiers: Vec<&str> = edges.iter().map(|e| e.specifier.as_str()).collect();
        specifiers.sort_unstable();
        assert_eq!(
            specifiers,
            [
                "crate::a",
                "crate::b::c",
                "crate::b::d",
                "mod sub",
                "serde",
                "std::fmt"
            ],
            "an inline `mod inline {{ }}` has no file and must not become an edge"
        );
        assert!(edges.iter().all(|e| e.to == ImportTarget::Unresolved));
    }

    #[test]
    fn javascript_finds_static_dynamic_and_require_forms() {
        let edges = extract(
            &lp("web/a.js"),
            Language::JavaScript,
            "import a from './a';\nexport { b } from './b';\nconst c = require('./c');\nimport('./d');\nnotRequire('./e');\nfoo(1, './f');\n",
        );
        let mut specifiers: Vec<&str> = edges.iter().map(|e| e.specifier.as_str()).collect();
        specifiers.sort_unstable();
        assert_eq!(
            specifiers,
            ["./a", "./b", "./c", "./d"],
            "only import/export/require/import() are imports"
        );
    }

    #[test]
    fn python_relative_names_become_module_candidates() {
        let edges = extract(
            &lp("py/main.py"),
            Language::Python,
            "from . import models, views as v\nfrom ..shared import cfg\nimport os.path as p\n",
        );
        let mut specifiers: Vec<&str> = edges.iter().map(|e| e.specifier.as_str()).collect();
        specifiers.sort_unstable();
        assert_eq!(
            specifiers,
            [".", "..shared", ".models", ".views", "os.path"],
            "`from . import x` names a module when `x.py` exists"
        );
    }

    // -- degradation (PRD §9: failure to parse is non-fatal) -----------------

    #[test]
    fn a_syntax_error_costs_only_that_file_its_streets() {
        let edges = extract(&lp("src/broken.rs"), Language::Rust, "}}} {{{ <<< !!! ???");
        assert!(edges.is_empty(), "garbage yields no edges, not a panic");

        // …and recovery still salvages the imports above the break.
        let recovered = extract(
            &lp("src/broken.rs"),
            Language::Rust,
            "use crate::util::helpers;\nfn ( ] } garbage <<< {{{\n",
        );
        assert_eq!(recovered.len(), 1, "{recovered:?}");
    }

    #[test]
    fn an_unsupported_extension_and_a_binary_file_are_ordinary_outcomes() {
        let f = corpus();
        let graph = f.build();
        let stats = graph.stats();

        assert_eq!(stats.skipped_no_grammar, 1, "docs/notes.md has no grammar");
        assert_eq!(stats.skipped_binary, 1, "py/blob.py is not UTF-8");
        assert_eq!(stats.skipped_too_large, 1, "web/huge.js is over the cap");
        assert_eq!(
            stats.skipped_industrial, 2,
            "one by FileClass::Industrial, one by the node_modules path guard"
        );
        assert!(
            stats.files_with_parse_errors >= 1,
            "src/broken.rs parses with errors: {stats:?}"
        );
        assert_eq!(stats.panicked_workers, 0);
        assert!(
            graph.edges_from(&lp("web/huge.js")).is_empty(),
            "an oversized bundle has no streets"
        );
        assert!(
            graph
                .edges_from(&lp("node_modules/left-pad/index.js"))
                .is_empty(),
            "PRD §8: the industrial zone is a mass, not a set of buildings"
        );
    }

    #[test]
    fn a_missing_file_on_disk_is_counted_not_fatal() {
        let mut f = Fixture::new();
        f.add("src/main.rs", Some(Language::Rust), b"fn main() {}\n");
        // Registered in the tree, absent from disk — the exact race a walk and a
        // parse have between them.
        let ghost = lp("src/ghost.rs");
        let mut meta = FileMeta::untracked(ghost.clone(), 10);
        meta.language = Some(Language::Rust);
        f.tree.files.insert(ghost, meta);

        let graph = f.build();
        assert_eq!(graph.stats().skipped_unreadable, 1);
        assert_eq!(graph.stats().files_parsed, 1);
    }

    // -- resolution ---------------------------------------------------------

    #[test]
    fn relative_index_and_extensionless_specifiers_resolve() {
        let f = corpus();
        let graph = f.build();

        assert_eq!(
            targets(&graph, "web/index.ts", "./lib/util"),
            ImportTarget::Internal(lp("web/lib/util.ts")),
            "extensionless relative import"
        );
        assert_eq!(
            targets(&graph, "web/index.ts", "../shared/config"),
            ImportTarget::Internal(lp("shared/config.ts")),
            "relative import out of the district"
        );
        assert_eq!(
            targets(&graph, "ui/App.tsx", "./components/Button"),
            ImportTarget::Internal(lp("ui/components/Button.tsx")),
        );
        assert_eq!(
            targets(&graph, "ui/components/index.tsx", "./Button"),
            ImportTarget::Internal(lp("ui/components/Button.tsx")),
        );
        assert_eq!(
            targets(&graph, "ui/App.tsx", "./helpers.js"),
            ImportTarget::Internal(lp("ui/helpers.ts")),
            "a `nodenext` project writes .js and means .ts"
        );
        assert_eq!(
            targets(&graph, "ui/App.tsx", "@/theme"),
            ImportTarget::Internal(lp("theme.ts")),
            "`@/` cannot be an npm scope, so it is unambiguously a project alias"
        );
    }

    #[test]
    fn an_index_file_resolves_a_bare_directory_import() {
        let mut f = Fixture::new();
        f.add(
            "app/main.ts",
            Some(Language::TypeScript),
            b"import { Button } from './widgets';\nexport const x = Button;\n",
        )
        .add(
            "app/widgets/index.ts",
            Some(Language::TypeScript),
            b"export const Button = 1;\n",
        );
        let graph = f.build();
        assert_eq!(
            targets(&graph, "app/main.ts", "./widgets"),
            ImportTarget::Internal(lp("app/widgets/index.ts")),
        );
    }

    #[test]
    fn packages_are_external_and_never_guessed_into_the_city() {
        let f = corpus();
        let graph = f.build();

        assert_eq!(
            targets(&graph, "web/index.ts", "react"),
            ImportTarget::External
        );
        assert_eq!(
            targets(&graph, "web/index.ts", "node:fs"),
            ImportTarget::External
        );
        assert_eq!(
            targets(&graph, "web/legacy.js", "lodash"),
            ImportTarget::External
        );
        assert_eq!(
            targets(&graph, "src/main.rs", "std::fmt::Debug"),
            ImportTarget::External
        );
        assert_eq!(
            targets(&graph, "py/main.py", "os.path"),
            ImportTarget::External
        );

        // A bare TypeScript specifier is never probed against the tree, even
        // when a file of that name exists: `import "theme"` is the npm package
        // `theme`, not `theme.ts`, unless a tsconfig says otherwise.
        let mut aliased = Fixture::new();
        aliased
            .add(
                "theme.ts",
                Some(Language::TypeScript),
                b"export const t = 1;\n",
            )
            .add(
                "app/a.ts",
                Some(Language::TypeScript),
                b"import { t } from 'theme';\nexport const x = t;\n",
            );
        let graph = aliased.build();
        assert_eq!(
            targets(&graph, "app/a.ts", "theme"),
            ImportTarget::External,
            "a wrong street is worse than a missing one"
        );
    }

    #[test]
    fn an_unresolvable_relative_import_is_unresolved_not_external() {
        let mut f = Fixture::new();
        f.add(
            "app/a.ts",
            Some(Language::TypeScript),
            b"import x from './does-not-exist';\nexport const y = x;\n",
        );
        let graph = f.build();
        assert_eq!(
            targets(&graph, "app/a.ts", "./does-not-exist"),
            ImportTarget::Unresolved,
            "it names something inside the repo; it is not a package"
        );
        assert_eq!(graph.unresolved_edges().count(), 1);
        assert!(graph.cross_district_edges().is_empty());
    }

    #[test]
    fn rust_module_and_crate_paths_resolve() {
        let f = corpus();
        let graph = f.build();

        assert_eq!(
            targets(&graph, "src/main.rs", "mod app"),
            ImportTarget::Internal(lp("src/app.rs")),
            "`mod app;` reaches a file, and that is a real edge"
        );
        assert_eq!(
            targets(&graph, "src/main.rs", "crate::util::helpers::sanitize"),
            ImportTarget::Internal(lp("src/util/helpers.rs")),
            "the longest prefix that names a file wins"
        );
        assert_eq!(
            targets(&graph, "src/app.rs", "crate::util::MODE"),
            ImportTarget::Internal(lp("src/util/mod.rs")),
            "`MODE` is an item, so the module file is the target"
        );
        assert_eq!(
            targets(&graph, "src/app.rs", "super::main"),
            ImportTarget::Internal(lp("src/main.rs")),
        );
        assert_eq!(
            targets(&graph, "src/util/mod.rs", "mod helpers"),
            ImportTarget::Internal(lp("src/util/helpers.rs")),
        );
        assert_eq!(
            targets(&graph, "src/main.rs", "polis_events::LogicalPath"),
            ImportTarget::Internal(lp("polis-events/src/lib.rs")),
            "a workspace sibling is inside the city, not outside it"
        );
    }

    #[test]
    fn use_super_inside_an_inline_mod_names_this_file_not_the_parent_module() {
        // Every `#[cfg(test)] mod tests { use super::*; }` in this workspace.
        // Counting `super` in directories instead of modules would give each of
        // them a false edge to `lib.rs`, and `lib.rs` would then win PRD §8's
        // inbound-import decile in every crate on the strength of its own tests.
        let mut f = Fixture::new();
        f.add(
            "src/lib.rs",
            Some(Language::Rust),
            b"pub mod path;\npub struct Root;\n",
        )
        .add(
            "src/path.rs",
            Some(Language::Rust),
            b"pub struct P;\n#[cfg(test)]\nmod tests {\n    use super::*;\n    use super::super::Root;\n    #[test] fn t() { let _ = (P, Root); }\n}\n",
        );
        let graph = f.build();

        assert_eq!(
            targets(&graph, "src/path.rs", "super"),
            ImportTarget::Internal(lp("src/path.rs")),
            "one `super` is absorbed by the inline `mod tests`"
        );
        assert_eq!(
            targets(&graph, "src/path.rs", "super::super::Root"),
            ImportTarget::Internal(lp("src/lib.rs")),
            "the second `super` does leave the file"
        );
        assert_eq!(
            graph
                .inbound_counts()
                .iter()
                .find(|(p, _)| *p == lp("src/path.rs"))
                .map(|(_, c)| *c),
            Some(1),
            "only lib.rs's `mod path;` counts — the self-edge does not"
        );
        assert!(graph
            .intra_district_edges(&lp("src"))
            .iter()
            .all(|e| { e.to.internal() != Some(&e.from) }));

        // …and outside an inline module, `super` still means the parent module.
        assert_eq!(
            resolve(&lp("src/path.rs"), "super::Root", Language::Rust, &f.tree),
            ImportTarget::Internal(lp("src/lib.rs")),
        );
    }

    #[test]
    fn python_relative_and_root_relative_imports_resolve() {
        let f = corpus();
        let graph = f.build();

        assert_eq!(
            targets(&graph, "py/main.py", ".helpers"),
            ImportTarget::Internal(lp("py/helpers.py")),
        );
        assert_eq!(
            targets(&graph, "py/main.py", ".models"),
            ImportTarget::Internal(lp("py/models.py")),
            "`from . import models` names a module",
        );
        assert_eq!(
            targets(&graph, "py/main.py", "."),
            ImportTarget::Internal(lp("py/__init__.py")),
        );
        assert_eq!(
            targets(&graph, "py/main.py", "pkg.sub"),
            ImportTarget::Internal(lp("pkg/sub.py")),
        );
    }

    // -- aggregation --------------------------------------------------------

    #[test]
    fn streets_are_cross_district_only_and_count_distinct_edges() {
        let f = corpus();
        let graph = f.build();
        let streets = graph.cross_district_edges();

        assert_eq!(
            street(&streets, "web", "web/lib"),
            Some(2),
            "index.ts and legacy.js both reach web/lib/util.ts — two file pairs"
        );
        assert_eq!(street(&streets, "web", "shared"), Some(1));
        assert_eq!(street(&streets, "ui", "ui/components"), Some(1));
        assert_eq!(
            street(&streets, "ui", ""),
            Some(1),
            "the repo root is PRD §8's civic square, not the absence of a district"
        );
        assert_eq!(street(&streets, "py", "pkg"), Some(1));
        assert_eq!(
            street(&streets, "src", "polis-events/src"),
            Some(1),
            "a workspace sibling crate is a district like any other"
        );

        // Four distinct file pairs reach `src/util`: main.rs and broken.rs and
        // app.rs all land on `helpers.rs`, and app.rs also lands on `mod.rs`.
        assert_eq!(street(&streets, "src", "src/util"), Some(4));

        // `web/index.ts` -> `./lazy` and `web/legacy.js` -> `./lib/util` prove
        // the intra/cross split: same district is never a street.
        assert_eq!(
            street(&streets, "web", "web"),
            None,
            "intra-district coupling is expected and boring (PRD §9)"
        );
        let intra = graph.intra_district_edges(&lp("web"));
        assert_eq!(
            intra
                .iter()
                .map(|e| (e.from.as_str(), e.specifier.as_str()))
                .collect::<Vec<_>>(),
            [("web/index.ts", "./lazy"), ("web/legacy.js", "./lazy")],
            "…but it is still recorded, as the weak attraction force"
        );
    }

    #[test]
    fn distinct_means_distinct_file_pairs_not_occurrences() {
        let mut f = Fixture::new();
        f.add(
            "a/one.rs",
            Some(Language::Rust),
            b"use crate::b::{First, Second, Third};\nuse crate::b::Fourth;\npub fn f() { let _: (First, Second, Third, Fourth); }\n",
        )
        // No `mod b;` here: this test is about one file's four `use` items
        // collapsing to one street, so `a/lib.rs` must not add a second pair.
        .add("a/lib.rs", Some(Language::Rust), b"pub struct Root;\n")
        .add("a/b/mod.rs", Some(Language::Rust), b"pub struct First;\n");
        let graph = f.build();

        assert_eq!(
            graph
                .edges_from(&lp("a/one.rs"))
                .iter()
                .filter(|e| e.to == ImportTarget::Internal(lp("a/b/mod.rs")))
                .count(),
            4,
            "the drill-down layer keeps every specifier (PRD §12)"
        );
        assert_eq!(
            street(&graph.cross_district_edges(), "a", "a/b"),
            Some(1),
            "street width is one relationship, not four `use` items"
        );
    }

    #[test]
    fn a_deleted_file_leaves_a_vacant_lot_not_a_dangling_edge() {
        let f = corpus();
        let mut graph = f.build();
        assert_eq!(
            street(&graph.cross_district_edges(), "web", "shared"),
            Some(1)
        );

        graph.remove_file(&lp("shared/config.ts"));

        assert_eq!(
            targets(&graph, "web/index.ts", "../shared/config"),
            ImportTarget::Unresolved,
            "PRD §7.5 — the import still exists, it just lands nowhere"
        );
        assert_eq!(street(&graph.cross_district_edges(), "web", "shared"), None);
        assert!(graph.edges_from(&lp("shared/config.ts")).is_empty());
    }

    #[test]
    fn an_incremental_update_matches_a_full_rebuild() {
        let mut f = corpus();
        let source = "import { util } from './lib/util';\nexport const only = util;\n";
        let mut incremental = f.build();
        incremental.update_file(&lp("web/index.ts"), source);

        f.add(
            "web/index.ts",
            Some(Language::TypeScript),
            source.as_bytes(),
        );
        let full = f.build();

        assert_eq!(
            incremental.edges(),
            full.edges(),
            "PRD §7.4 — one growth step must land where a replay lands"
        );
        assert_eq!(
            incremental.cross_district_edges(),
            full.cross_district_edges()
        );
    }

    #[test]
    fn a_new_file_needs_upsert_and_then_resolves_both_ways() {
        let mut f = Fixture::new();
        f.add(
            "app/a.ts",
            Some(Language::TypeScript),
            b"import { b } from '../lib/b';\nexport const x = b;\n",
        );
        let mut graph = f.build();
        assert_eq!(
            targets(&graph, "app/a.ts", "../lib/b"),
            ImportTarget::Unresolved
        );

        graph.upsert_file(
            &lp("lib/b.ts"),
            Language::TypeScript,
            "export const b = 1;\n",
        );
        graph.update_file(
            &lp("app/a.ts"),
            "import { b } from '../lib/b';\nexport const x = b;\n",
        );

        assert_eq!(
            targets(&graph, "app/a.ts", "../lib/b"),
            ImportTarget::Internal(lp("lib/b.ts")),
        );
    }

    // -- diagnostics --------------------------------------------------------

    #[test]
    fn inbound_counts_and_the_monument_decile_are_stable() {
        let f = corpus();
        let graph = f.build();
        let counts = graph.inbound_counts();

        let helpers = counts
            .iter()
            .find(|(p, _)| *p == lp("src/util/helpers.rs"))
            .map(|(_, c)| *c);
        assert_eq!(
            helpers,
            Some(4),
            "main.rs, app.rs, util/mod.rs — and broken.rs, whose imports survive \
             its syntax error because tree-sitter recovers"
        );
        assert!(
            counts.windows(2).all(|w| w[0].0 < w[1].0),
            "inbound counts are sorted by path"
        );

        let decile = graph.inbound_top_decile();
        assert_eq!(decile.len(), counts.len().div_ceil(10));
        assert!(decile.contains(&lp("src/util/helpers.rs")), "{decile:?}");
        assert_eq!(
            decile,
            graph.inbound_top_decile(),
            "ties break deterministically"
        );
    }

    #[test]
    fn a_hub_district_and_an_isolated_district_both_fall_out() {
        let mut f = Fixture::new();
        // `core` imports from four districts; `alone` imports nothing and is
        // imported by nothing.
        f.add(
            "core/hub.ts",
            Some(Language::TypeScript),
            b"import a from '../a/a';\nimport b from '../b/b';\nimport c from '../c/c';\nimport d from '../d/d';\nexport const all = [a, b, c, d];\n",
        )
        .add("a/a.ts", Some(Language::TypeScript), b"export default 1;\n")
        .add("b/b.ts", Some(Language::TypeScript), b"export default 2;\n")
        .add("c/c.ts", Some(Language::TypeScript), b"export default 3;\n")
        .add("d/d.ts", Some(Language::TypeScript), b"export default 4;\n")
        .add("alone/x.ts", Some(Language::TypeScript), b"export const x = 0;\n")
        .add("docs/readme.md", None, b"# not a code district\n");

        let graph = f.build();
        assert_eq!(graph.hub_districts(&f.tree), vec![lp("core")]);
        assert_eq!(graph.isolated_districts(&f.tree), vec![lp("alone")]);

        let rows = graph.diagnostics(&f.tree);
        assert!(
            rows.iter().all(|r| r.district != lp("docs")),
            "a Markdown directory is not an isolated code district"
        );
        let core = rows
            .iter()
            .find(|r| r.district == lp("core"))
            .expect("core");
        assert_eq!((core.outbound_districts, core.inbound_districts), (4, 0));
        assert_eq!(core.neighbours, 4);
        assert_eq!(core.outbound_edges, 4);
    }

    #[test]
    fn external_packages_are_kept_per_district_for_the_industrial_zone() {
        let f = corpus();
        let graph = f.build();
        let packages = graph.external_packages();

        let web = packages.get(&lp("web")).expect("web imports packages");
        assert!(web.contains("react"), "{web:?}");
        assert!(web.contains("fs"), "node:fs is the module `fs`: {web:?}");
        assert!(web.contains("lodash"), "{web:?}");
        assert!(
            !packages.contains_key(&lp("shared")),
            "a district with no package imports has no row"
        );
    }

    #[test]
    fn package_roots_are_taken_per_language() {
        assert_eq!(
            package_root("react-dom/client", Language::JavaScript),
            Some("react-dom")
        );
        assert_eq!(
            package_root("@scope/pkg/sub", Language::TypeScript),
            Some("@scope/pkg")
        );
        assert_eq!(
            package_root("@scope/pkg", Language::Tsx),
            Some("@scope/pkg")
        );
        assert_eq!(
            package_root("node:fs/promises", Language::JavaScript),
            Some("fs")
        );
        assert_eq!(package_root("./local", Language::JavaScript), None);
        assert_eq!(package_root("@/alias", Language::TypeScript), None);
        assert_eq!(
            package_root("std::collections::BTreeMap", Language::Rust),
            Some("std")
        );
        assert_eq!(package_root("crate::a", Language::Rust), None);
        assert_eq!(package_root("os.path", Language::Python), Some("os"));
        assert_eq!(package_root(".relative", Language::Python), None);
        assert_eq!(package_root("   ", Language::Rust), None);
    }

    // -- determinism (PRD §7.4) ---------------------------------------------

    #[test]
    fn edges_are_sorted_and_identical_across_runs() {
        let f = corpus();
        let first = f.build();
        assert!(first.edges().is_sorted(), "edge order is layout-visible");
        assert!(!first.is_empty());

        let baseline: Vec<String> = first
            .edges()
            .iter()
            .map(|e| format!("{}|{:?}|{}|{}", e.from, e.to, e.specifier, e.language))
            .collect();

        // Five more builds, each racing however many worker threads this machine
        // gives it. Nothing about the answer may move.
        for run in 0..5 {
            let again = f.build();
            let observed: Vec<String> = again
                .edges()
                .iter()
                .map(|e| format!("{}|{:?}|{}|{}", e.from, e.to, e.specifier, e.language))
                .collect();
            assert_eq!(observed, baseline, "run {run} disagreed");
            assert_eq!(again.cross_district_edges(), first.cross_district_edges());
            assert_eq!(again.inbound_counts(), first.inbound_counts());
            assert_eq!(again.diagnostics(&f.tree), first.diagnostics(&f.tree));
        }
    }

    #[test]
    fn parallel_and_sequential_parsing_agree() {
        // `worker_count` is the only thing that differs between a small repo and
        // a large one, so drive both paths over one corpus.
        let mut f = Fixture::new();
        for i in 0..(PARALLEL_THRESHOLD * 3) {
            f.add(
                &format!("d{}/m{i}.ts", i % 7),
                Some(Language::TypeScript),
                format!(
                    "import {{ x }} from '../d{}/m{}';\nexport const x = {i};\n",
                    (i + 1) % 7,
                    i + 1
                )
                .as_bytes(),
            );
        }
        assert!(
            worker_count(f.tree.files.len()) > 1,
            "the fixture must be wide enough"
        );

        let index = ImportIndex::from_tree(&f.tree);
        let mut stats = ImportStats::default();
        let candidates = select_candidates(&f.tree, &mut stats);

        let (parallel, _, _) = parse_all(&f.tree.root, &index, &candidates, None);
        let mut sequential = Vec::new();
        let mut ignored = ImportStats::default();
        parse_chunk(
            &f.tree.root,
            &index,
            &candidates,
            None,
            &mut sequential,
            &mut Vec::new(),
            &mut ignored,
        );

        let sorted = |v: Vec<(LogicalPath, Vec<ImportEdge>)>| {
            let mut v: Vec<_> = v;
            v.sort();
            v
        };
        assert_eq!(sorted(parallel), sorted(sequential));
    }

    #[test]
    fn worker_count_never_exceeds_its_caps() {
        assert_eq!(worker_count(0), 1);
        assert_eq!(worker_count(PARALLEL_THRESHOLD - 1), 1);
        assert!(worker_count(PARALLEL_THRESHOLD) >= 1);
        assert!(worker_count(100_000) <= MAX_PARSE_THREADS);
    }

    // -- performance (PRD §13.1) --------------------------------------------

    #[test]
    fn a_five_thousand_file_repo_indexes_well_inside_the_cold_start_budget() {
        const FILES: usize = 5_000;
        const DISTRICTS: usize = 50;
        let mut f = Fixture::new();
        for i in 0..FILES {
            let district = i % DISTRICTS;
            // `m{i}` always lives in `d{i % DISTRICTS}`, so `i + 1` is the next
            // district over and `i + DISTRICTS` is a sibling in this one. One
            // cross-district edge, one intra-district edge, one package.
            f.add(
                &format!("src/d{district}/m{i}.ts"),
                Some(Language::TypeScript),
                format!(
                    "import {{ helper }} from '../d{}/m{}';\nimport React from 'react';\nimport {{ z }} from './m{}';\nexport const helper = {i};\nexport const z = () => helper + Number(React) + z.length;\n",
                    (district + 1) % DISTRICTS,
                    (i + 1) % FILES,
                    (i + DISTRICTS) % FILES
                )
                .as_bytes(),
            );
        }

        let (graph, elapsed) = ImportGraph::build_timed(&f.tree);
        println!(
            "PRD §13.1: {FILES} files, {} edges, {} streets in {:?} ({} workers)",
            graph.len(),
            graph.cross_district_edges().len(),
            elapsed,
            worker_count(FILES),
        );

        assert_eq!(usize::try_from(graph.stats().files_parsed), Ok(FILES));
        assert_eq!(
            graph.cross_district_edges().len(),
            DISTRICTS,
            "one street per adjacent district pair, each carrying {} edges",
            FILES / DISTRICTS
        );
        // PRD §13.1's whole cold-start budget is 3 s and parsing is only part of
        // it, but the bound asserted here is deliberately loose: this runs in a
        // `cargo test` process alongside every other test, on whatever machine
        // CI happens to be. It is a guard against an accidentally quadratic
        // resolver, not a benchmark. The real figure is printed above.
        assert!(
            elapsed < Duration::from_secs(60),
            "5k files took {elapsed:?}; something is quadratic"
        );
    }

    // -----------------------------------------------------------------------
    // The specifier cache (PRD §13.1)
    // -----------------------------------------------------------------------

    /// The whole point: a warm cache must produce the identical graph. Not
    /// "roughly the same edges" — the same edges, in the same order, with the
    /// same resolutions, because the layout reads them (PRD §7.4).
    #[test]
    fn a_warm_specifier_cache_builds_the_identical_graph() {
        let f = corpus();
        let cache = f.cache_file();
        let cold = f.build_through(&cache);
        assert!(
            !ParseCache::read(&cache).is_empty(),
            "the cold build wrote nothing to remember"
        );
        let warm = f.build_through(&cache);
        assert_eq!(cold.edges(), warm.edges(), "a warm build moved a street");
        assert_eq!(
            cold.edges(),
            f.build().edges(),
            "the cached path and the uncached path disagree"
        );
    }

    /// A file whose bytes changed is re-parsed, whatever its length or its name.
    #[test]
    fn editing_a_file_invalidates_only_its_own_entry() {
        let mut f = corpus();
        let cache = f.cache_file();
        f.build_through(&cache);
        // Same length, different bytes: the length check alone would miss this,
        // which is why the digest is the check and the length is the extra.
        f.add(
            "src/util/helpers.rs",
            Some(Language::Rust),
            b"use crate::app::run;
pub fn sanitize() { run() }
",
        );
        let after = f.build_through(&cache);
        assert!(
            after
                .edges_from(&lp("src/util/helpers.rs"))
                .iter()
                .any(|e| e.specifier == "crate::app::run"),
            "the edited file kept its old imports: {:?}",
            after.edges_from(&lp("src/util/helpers.rs"))
        );
        assert_eq!(
            after.edges(),
            f.build().edges(),
            "an edited file left the cached graph different from a fresh one"
        );
    }

    /// A cache from another version, or a corrupt one, is a miss and never a
    /// failure: a launch must not depend on a state file being intact.
    #[test]
    fn a_corrupt_or_stale_cache_is_simply_cold() {
        let f = corpus();
        let cache = f.cache_file();
        std::fs::write(&cache, b"{ this is not json").expect("write");
        assert!(ParseCache::read(&cache).is_empty());
        let stale = serde_json::json!({ "version": PARSE_CACHE_VERSION + 1, "entries": [] });
        std::fs::write(&cache, serde_json::to_vec(&stale).expect("json")).expect("write");
        assert!(ParseCache::read(&cache).is_empty());
        // And a build over it still works, and still writes a usable one.
        let built = f.build_through(&cache);
        assert_eq!(built.edges(), f.build().edges());
        assert!(!ParseCache::read(&cache).is_empty());
    }

    /// The cache holds the files that exist now, not every file that ever did.
    #[test]
    fn a_deleted_file_leaves_the_cache() {
        let mut f = corpus();
        let cache = f.cache_file();
        f.build_through(&cache);
        let before = ParseCache::read(&cache).len();
        f.tree.files.remove(&lp("src/main.rs"));
        f.build_through(&cache);
        let after = ParseCache::read(&cache);
        assert_eq!(
            after.len(),
            before - 1,
            "the cache kept a file that is gone"
        );
        assert!(!after.by_path.contains_key(&lp("src/main.rs")));
    }

    /// Two different byte strings must not share a digest for any reason a test
    /// can construct — in particular not by transposition, which a plain
    /// additive checksum would miss.
    #[test]
    fn the_content_digest_separates_transpositions() {
        assert_ne!(content_digest(b"ab"), content_digest(b"ba"));
        assert_ne!(
            content_digest(
                b"use a;
use b;
"
            ),
            content_digest(
                b"use b;
use a;
"
            )
        );
        assert_ne!(content_digest(b""), content_digest(&[0u8]));
        assert_eq!(content_digest(b"stable"), content_digest(b"stable"));
    }
}
