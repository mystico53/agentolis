//! Identity, fingerprints, drift, and staleness as a state the renderer can
//! draw.
//!
//! # The question this module answers
//!
//! *When does a district need a new description?*
//!
//! The obvious answer — "when its contents change" — is wrong, and expensively
//! so. Keyed on content, every commit invalidates most of the cache: a one-time
//! cost becomes a per-commit cost, and worse, **the words on the map churn**.
//! Spatial memory is the entire product (PRD §7.4); a caption that rewrites
//! itself every afternoon is the same failure as a city that reshuffles.
//!
//! So:
//!
//! > **Editing a file does not change what a folder is.** You can rewrite every
//! > line in `src/auth` and it is still the authentication code.
//!
//! The key is therefore the district's **identity** — its path — plus a
//! fingerprint of its **file names**. Never file contents. A description is
//! regenerated only when meaning plausibly moved, which is three things and no
//! others:
//!
//! 1. **A district appeared** that has no entry ([`Freshness::Missing`]).
//! 2. **A district split or merged.** This is the one that is easy to miss and
//!    is why [`CachedModelDescription`] stores the child district paths as well
//!    as the names. District granularity is adaptive (ADR-0086): a growing
//!    `src/services` splits into `src/services/auth` and
//!    `src/services/billing`, which orphans the old description and creates two
//!    undescribed districts. The parent's own file names may barely have moved
//!    while what the parent *is* changed completely.
//! 3. **The name fingerprint drifted** past
//!    [`crate::llm::LlmConfig::drift_threshold_permille`].
//!
//! Relations are not on that list on purpose. Import edges are computed exactly
//! by the tree-sitter graph and update instantly; they answer "what talks to
//! what". Descriptions answer "what is this folder", which moves slowly. Keeping
//! them apart is what stops the fast-changing half from ever triggering a call.
//!
//! # Staleness is visible, never silent
//!
//! A description reading "Payment processing" for a folder that quietly became
//! the notification service is **worse than no description**: it is confidently
//! wrong, and the operator would trust it. So drift is not a boolean that
//! silently triggers work — it is [`Freshness`], carried on the neighborhood,
//! and a renderer that draws a stale caption the same as a fresh one is choosing
//! to.
//!
//! # The sketch
//!
//! Drift has to be computable from the cache alone, with no call and no second
//! walk, so the name set is stored. Storing thousands of names per district is
//! not free, so [`Sketch`] keeps the *k* smallest hashes of the set: **exact
//! Jaccard below [`Sketch::CAPACITY`] names, a k-minimum-values estimate above
//! it**. Every civic district in the measured corpus is below the cap; the
//! estimator exists for `vc-tower`'s 4 418-file scrape directories.
//!
//! The hash is written out here rather than taken from `DefaultHasher`, for the
//! reason ADR-0029 gives: two machines must agree about a hit.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use polis_events::LogicalPath;
use serde::{Deserialize, Serialize};

use crate::describe::Description;

// ---------------------------------------------------------------------------
// Freshness
// ---------------------------------------------------------------------------

/// Why a description is no longer trustworthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftCause {
    /// The district's own file names moved past the threshold.
    Names,
    /// A district that did not exist when this was written now sits inside
    /// this one: the district gave part of itself away.
    Split,
    /// A district that existed inside this one when this was written is gone:
    /// its files folded back in.
    Merge,
    /// The model or the prompt changed, so the same input would not produce the
    /// same answer.
    Recipe,
}

impl DriftCause {
    /// A stable name for reports.
    pub fn name(self) -> &'static str {
        match self {
            Self::Names => "names",
            Self::Split => "split",
            Self::Merge => "merge",
            Self::Recipe => "recipe",
        }
    }
}

/// How far a description has drifted from the district it describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Drift {
    /// Fraction of the district's file names that changed, in parts per
    /// thousand.
    ///
    /// An integer, not an `f32`, for two reasons: [`crate::neighborhoods::
    /// Neighborhood`] derives `Eq`, and a value that reaches a serialized
    /// artifact must compare bit-identically on every machine (PRD §7.4).
    pub permille: u16,
    /// What moved.
    pub cause: DriftCause,
}

impl Drift {
    /// The drift as a fraction in `0.0..=1.0`, for a renderer.
    #[allow(clippy::cast_precision_loss)] // 0..=1000 is exact in f32
    pub fn fraction(self) -> f32 {
        f32::from(self.permille) / 1000.0
    }
}

/// How current a district's model-written description is (PRD §12).
///
/// The state a renderer needs to avoid presenting a confidently wrong caption
/// as current. `Missing` is the default and is also the answer for every
/// district whose description is a quotation from the repository — those do not
/// go stale in this sense, because [`crate::describe`] re-derives them from the
/// file every run.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    /// No model-written description: never generated, or the description on
    /// this district came from the repository itself.
    #[default]
    Missing,
    /// Written by the model, and the district still looks like the one it was
    /// written for.
    Fresh,
    /// Written by the model, and the district has moved since. **Draw this
    /// differently.**
    Stale(Drift),
}

impl Freshness {
    /// True when a renderer should mark the caption.
    pub fn is_stale(self) -> bool {
        matches!(self, Self::Stale(_))
    }

    /// The drift, when there is any.
    pub fn drift(self) -> Option<Drift> {
        match self {
            Self::Stale(d) => Some(d),
            Self::Fresh | Self::Missing => None,
        }
    }

    /// A stable name for reports.
    pub fn name(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Fresh => "fresh",
            Self::Stale(_) => "stale",
        }
    }
}

/// The shipped drift threshold, in parts per thousand.
///
/// **440 ‰ of Jaccard distance.** The brief asked for "roughly a third of names
/// changed"; this is that, converted into the metric actually used and then
/// checked against real history. See `examples/llm_drift.rs`, which measures
/// every district of every repository in the operator's `C:/coding` at `HEAD`
/// against the same repository 30, 90 and 365 days earlier, using `git ls-tree`
/// so no checkout moves.
///
/// The conversion matters and is the reason this is not `330`. Drift here is
/// Jaccard distance, `1 - |A ∩ B| / |A ∪ B|`, which is symmetric in additions
/// and removals — the property you want, because a district that loses a third
/// of its files has changed as much as one that gains a third. But replacing a
/// third of the names in a set of *n* gives `2n/3 ÷ 4n/3 = 0.50`, not `0.33`,
/// and adding a third gives `0.25`. "A third of names changed" therefore lands
/// between 250 ‰ and 500 ‰ depending on which third, and the measurement is what
/// picks the point inside that band.
pub const DEFAULT_DRIFT_THRESHOLD_PERMILLE: u16 = 440;

// ---------------------------------------------------------------------------
// The name sketch
// ---------------------------------------------------------------------------

/// A district's file names, as a set of hashes.
///
/// Names, never contents — see the module documentation. Exact below
/// [`Sketch::CAPACITY`]; a k-minimum-values estimate above it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sketch {
    /// How many distinct names went in, before any capping.
    pub count: u32,
    /// The smallest [`Sketch::CAPACITY`] hashes, ascending. `count` values when
    /// `count <= CAPACITY`.
    pub hashes: Vec<u64>,
}

/// FNV-1a with a `splitmix64` finalizer, written out.
///
/// Written out rather than taken from `DefaultHasher` for ADR-0029's reason: a
/// per-process seed makes two machines disagree about a hit. The finalizer is
/// not decoration — a k-minimum-values sketch selects on the *high* bits, and
/// FNV-1a's avalanche there is not good enough to make "smallest hash" a fair
/// sample of the set.
pub fn name_hash(name: &str) -> u64 {
    let mut z = crate::git::fnv1a64(name.as_bytes()).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Sketch {
    /// Names kept exactly. Above this the sketch estimates.
    ///
    /// 1 024 covers every civic district measured across eleven repositories —
    /// the largest was 223 files — with room for an order of magnitude. It is
    /// exceeded only by `vc-tower`'s scraped `partners_html` (4 418) and its
    /// three siblings, which is exactly the case an estimator is for.
    pub const CAPACITY: usize = 1024;

    /// Builds a sketch from a district's names.
    ///
    /// Duplicates collapse, and order does not matter — both required, because
    /// the caller's iteration order must never reach a cached value (PRD §7.4).
    pub fn build<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut set: BTreeSet<u64> = BTreeSet::new();
        for name in names {
            set.insert(name_hash(name.as_ref()));
        }
        let count = u32::try_from(set.len()).unwrap_or(u32::MAX);
        Self {
            count,
            hashes: set.into_iter().take(Self::CAPACITY).collect(),
        }
    }

    /// True when every name is represented exactly.
    pub fn is_exact(&self) -> bool {
        self.count as usize <= Self::CAPACITY
    }

    /// True when nothing went in.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Jaccard **distance** between two sketches: `0.0` identical, `1.0`
    /// disjoint.
    ///
    /// Exact when both sides are exact. Otherwise the standard k-minimum-values
    /// estimate: take the *k* smallest hashes of the union of the two sketches
    /// and ask what fraction of them are in both.
    #[allow(clippy::cast_precision_loss)] // set sizes are far below 2^53
    pub fn distance(&self, other: &Self) -> f64 {
        if self.is_empty() && other.is_empty() {
            return 0.0;
        }
        if self.is_empty() || other.is_empty() {
            return 1.0;
        }
        let a: BTreeSet<u64> = self.hashes.iter().copied().collect();
        let b: BTreeSet<u64> = other.hashes.iter().copied().collect();
        if self.is_exact() && other.is_exact() {
            let intersection = a.intersection(&b).count() as f64;
            let union = a.union(&b).count() as f64;
            return 1.0 - intersection / union;
        }
        let k = Self::CAPACITY.min(a.len()).min(b.len());
        if k == 0 {
            return 1.0;
        }
        let mut shared = 0usize;
        for h in a.union(&b).take(k) {
            if a.contains(h) && b.contains(h) {
                shared += 1;
            }
        }
        1.0 - shared as f64 / k as f64
    }

    /// [`Sketch::distance`] in parts per thousand, saturating and rounded.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to 0..=1000
    pub fn distance_permille(&self, other: &Self) -> u16 {
        (self.distance(other) * 1000.0).round().clamp(0.0, 1000.0) as u16
    }
}

// ---------------------------------------------------------------------------
// The cache
// ---------------------------------------------------------------------------

/// On-disk format version. A bump discards every entry.
///
/// Bump this when the *shape* changes. A change to the prompt or the model does
/// **not** need a bump, because [`CachedModelDescription`] records both and
/// [`ModelCache::freshness`] reports a mismatch as [`DriftCause::Recipe`] — that
/// is finer-grained, and it keeps a warm cache useful across a prompt tweak
/// instead of throwing away a repository's worth of paid-for text.
pub const MODEL_CACHE_VERSION: u32 = 1;

/// One district's model-written description, and everything needed to decide
/// whether it still applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedModelDescription {
    /// The district. The identity half of the key.
    pub path: LogicalPath,
    /// Its file names when the text was written. The fingerprint half.
    pub sketch: Sketch,
    /// The districts that sat inside it at that moment, in path order. The
    /// split/merge detector.
    pub children: Vec<LogicalPath>,
    /// What the model said. `None` is cached deliberately: "I have nothing to
    /// say about this directory" is an answer, it cost the same to get, and
    /// asking again would cost it again.
    pub description: Option<Description>,
    /// The model id that wrote it.
    pub model: String,
    /// The prompt version that asked for it.
    pub prompt_version: u32,
}

/// The on-disk file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelCacheFile {
    version: u32,
    entries: Vec<CachedModelDescription>,
}

/// What the model wrote last time, keyed on district identity plus a file-name
/// fingerprint.
///
/// Lives in the platform state directory — see
/// [`crate::llm::default_cache_path`] and ADR-0065 for why it must never be
/// inside the repository.
#[derive(Debug, Default, Clone)]
pub struct ModelCache {
    by_path: BTreeMap<LogicalPath, CachedModelDescription>,
    dirty: bool,
}

impl ModelCache {
    /// Reads a cache file, or an empty cache when there is none, it is
    /// unreadable, or a different version wrote it.
    ///
    /// Never an error: a cold cache and a corrupt one are the same situation,
    /// and the same situation as the feature being switched off.
    pub fn read(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else {
            return Self::default();
        };
        let Ok(file) = serde_json::from_slice::<ModelCacheFile>(&bytes) else {
            return Self::default();
        };
        if file.version != MODEL_CACHE_VERSION {
            return Self::default();
        }
        Self {
            by_path: file
                .entries
                .into_iter()
                .map(|e| (e.path.clone(), e))
                .collect(),
            dirty: false,
        }
    }

    /// Writes the cache.
    ///
    /// Failure is silent by design: an unwritable state directory costs the next
    /// run a regeneration, and nothing else. It must not be able to fail a
    /// render.
    pub fn write(&self, path: &Path) {
        let file = ModelCacheFile {
            version: MODEL_CACHE_VERSION,
            entries: self.by_path.values().cloned().collect(),
        };
        if let Ok(bytes) = serde_json::to_vec(&file) {
            if let Err(error) = crate::git::write_atomic(path, &bytes) {
                tracing::debug!(%error, "could not write the model description cache");
            }
        }
    }

    /// The entry for a district, whatever its freshness.
    pub fn get(&self, path: &LogicalPath) -> Option<&CachedModelDescription> {
        self.by_path.get(path)
    }

    /// Records what the model said about a district.
    pub fn put(&mut self, entry: CachedModelDescription) {
        self.by_path.insert(entry.path.clone(), entry);
        self.dirty = true;
    }

    /// How current the entry for `path` is, given what the district looks like
    /// now.
    ///
    /// The order of the tests is the design. Structure beats names: a split is
    /// reported even when the parent's own file names barely moved, because the
    /// parent gave away the part the description was about.
    pub fn freshness(
        &self,
        path: &LogicalPath,
        sketch: &Sketch,
        children: &[LogicalPath],
        model: &str,
        prompt_version: u32,
        threshold_permille: u16,
    ) -> Freshness {
        let Some(entry) = self.by_path.get(path) else {
            return Freshness::Missing;
        };
        let before: BTreeSet<&LogicalPath> = entry.children.iter().collect();
        let now: BTreeSet<&LogicalPath> = children.iter().collect();
        let permille = entry.sketch.distance_permille(sketch);
        if now.difference(&before).next().is_some() {
            return Freshness::Stale(Drift {
                permille,
                cause: DriftCause::Split,
            });
        }
        if before.difference(&now).next().is_some() {
            return Freshness::Stale(Drift {
                permille,
                cause: DriftCause::Merge,
            });
        }
        if entry.model != model || entry.prompt_version != prompt_version {
            return Freshness::Stale(Drift {
                permille,
                cause: DriftCause::Recipe,
            });
        }
        if permille >= threshold_permille {
            return Freshness::Stale(Drift {
                permille,
                cause: DriftCause::Names,
            });
        }
        Freshness::Fresh
    }

    /// Entries for districts that no longer exist.
    ///
    /// A split orphans the parent's description in the sense that it no longer
    /// describes what it used to; a *deletion* orphans it outright. Reported
    /// rather than silently dropped, because "we are holding paid-for text for
    /// eleven directories that are gone" is a fact about the cache.
    pub fn orphans(&self, live: &BTreeSet<LogicalPath>) -> Vec<LogicalPath> {
        self.by_path
            .keys()
            .filter(|p| !live.contains(*p))
            .cloned()
            .collect()
    }

    /// Drops entries for districts that no longer exist.
    pub fn prune(&mut self, live: &BTreeSet<LogicalPath>) -> usize {
        let before = self.by_path.len();
        self.by_path.retain(|p, _| live.contains(p));
        let removed = before - self.by_path.len();
        if removed > 0 {
            self.dirty = true;
        }
        removed
    }

    /// Forgets everything. `polis describe --clear-cache`.
    pub fn clear(&mut self) {
        if !self.by_path.is_empty() {
            self.dirty = true;
        }
        self.by_path.clear();
    }

    /// How many districts it holds.
    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    /// True when it holds nothing.
    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }

    /// True when something changed since it was read.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Every entry, in path order.
    pub fn entries(&self) -> impl Iterator<Item = &CachedModelDescription> {
        self.by_path.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::describe::DescriptionSource;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    fn names(n: usize, prefix: &str) -> Vec<String> {
        (0..n).map(|i| format!("{prefix}{i:04}.ts")).collect()
    }

    fn description(label: &str) -> Description {
        Description {
            label: label.to_owned(),
            detail: format!("{label} in detail."),
            source: DescriptionSource::Model,
            origin: None,
        }
    }

    fn entry(path: &str, names: &[String], children: &[&str]) -> CachedModelDescription {
        CachedModelDescription {
            path: lp(path),
            sketch: Sketch::build(names),
            children: children.iter().map(|c| lp(c)).collect(),
            description: Some(description("Payment processing")),
            model: "glm-5.3-flash".to_owned(),
            prompt_version: 1,
        }
    }

    // -- the sketch ---------------------------------------------------------

    #[test]
    fn a_sketch_is_order_independent_and_collapses_duplicates() {
        let a = Sketch::build(["a.ts", "b.ts", "c.ts"]);
        let b = Sketch::build(["c.ts", "a.ts", "b.ts", "a.ts"]);
        assert_eq!(a, b, "iteration order must never reach a cached value");
        assert_eq!(a.count, 3);
        assert!(a.is_exact());
        assert!((a.distance(&b)).abs() < f64::EPSILON);
    }

    #[test]
    fn the_hash_is_the_same_number_on_every_machine() {
        // Written out, not `DefaultHasher` (ADR-0029). Pinned so a change to the
        // function is a deliberate act that shows up as a failing test.
        assert_eq!(name_hash(""), 0xc381_7c01_6ba4_ff30);
        assert_ne!(name_hash("a.ts"), name_hash("b.ts"));
        // Adjacent names must not land adjacent, or the k-smallest sample is
        // not a sample.
        let a = name_hash("file0001.ts");
        let b = name_hash("file0002.ts");
        assert!(a.abs_diff(b) > 1_000_000, "{a:x} {b:x}");
    }

    #[test]
    fn drift_is_jaccard_distance_and_symmetric_in_adding_and_removing() {
        let base = names(30, "f");
        let sketch = Sketch::build(&base);
        assert_eq!(sketch.distance_permille(&sketch), 0, "identical is zero");

        // Ten added to thirty: 30 shared of 40 union.
        let grown = Sketch::build(names(40, "f"));
        assert_eq!(sketch.distance_permille(&grown), 250);
        // Ten removed from thirty: the same 30-of-40, the other way round.
        let shrunk = Sketch::build(names(20, "f"));
        assert_eq!(shrunk.distance_permille(&sketch), 333);

        // Wholly different names: disjoint.
        let other = Sketch::build(names(30, "g"));
        assert_eq!(sketch.distance_permille(&other), 1000);
        // And it is symmetric.
        assert_eq!(
            sketch.distance_permille(&grown),
            grown.distance_permille(&sketch)
        );
    }

    #[test]
    fn a_third_of_the_names_replaced_lands_inside_the_shipped_threshold() {
        // The brief said "roughly a third of names changed". A third *replaced*
        // in a set of 30 is 20 shared of 40 union = 500 ‰, which is past the
        // 440 ‰ threshold; a third *added* is 250 ‰, which is not. That band is
        // the reason the constant is not simply 330.
        let before = Sketch::build(names(30, "f"));
        let mut after: Vec<String> = names(20, "f");
        after.extend(names(10, "g"));
        let after = Sketch::build(&after);
        assert_eq!(before.distance_permille(&after), 500);
        assert!(before.distance_permille(&after) >= DEFAULT_DRIFT_THRESHOLD_PERMILLE);

        let added = Sketch::build(names(40, "f"));
        assert!(before.distance_permille(&added) < DEFAULT_DRIFT_THRESHOLD_PERMILLE);
    }

    #[test]
    fn a_district_larger_than_the_cap_is_estimated_and_stays_close() {
        let big: Vec<String> = names(Sketch::CAPACITY * 4, "f");
        let sketch = Sketch::build(&big);
        assert!(!sketch.is_exact());
        assert_eq!(sketch.hashes.len(), Sketch::CAPACITY);
        assert_eq!(sketch.count as usize, Sketch::CAPACITY * 4);
        assert_eq!(sketch.distance_permille(&sketch), 0);

        // Add a quarter again: true Jaccard distance is 1 - 4096/5120 = 0.20.
        let grown = Sketch::build(names(Sketch::CAPACITY * 5, "f"));
        let estimated = sketch.distance_permille(&grown);
        assert!(
            (150..=250).contains(&estimated),
            "estimate {estimated} is not near 200"
        );
    }

    #[test]
    fn an_empty_district_is_not_confused_with_a_changed_one() {
        let empty = Sketch::build(Vec::<String>::new());
        let full = Sketch::build(names(5, "f"));
        assert_eq!(empty.distance_permille(&empty), 0);
        assert_eq!(empty.distance_permille(&full), 1000);
        assert!(empty.is_empty());
    }

    // -- freshness ----------------------------------------------------------

    #[test]
    fn a_district_with_no_entry_is_missing_not_stale() {
        let cache = ModelCache::default();
        let f = cache.freshness(
            &lp("src/auth"),
            &Sketch::build(names(5, "f")),
            &[],
            "glm-5.3-flash",
            1,
            DEFAULT_DRIFT_THRESHOLD_PERMILLE,
        );
        assert_eq!(f, Freshness::Missing);
        assert!(!f.is_stale());
        assert_eq!(f.drift(), None);
    }

    /// The rule the whole design rests on: rewriting every line of a file must
    /// not cost a call, because the folder is still the same folder.
    #[test]
    fn editing_every_file_in_a_district_does_not_invalidate_its_description() {
        let mut cache = ModelCache::default();
        let files = names(40, "auth");
        cache.put(entry("src/auth", &files, &[]));
        // The contents changed completely; the names did not.
        let f = cache.freshness(
            &lp("src/auth"),
            &Sketch::build(&files),
            &[],
            "glm-5.3-flash",
            1,
            DEFAULT_DRIFT_THRESHOLD_PERMILLE,
        );
        assert_eq!(f, Freshness::Fresh);
    }

    #[test]
    fn a_district_whose_names_drift_past_the_threshold_is_stale_with_a_number() {
        let mut cache = ModelCache::default();
        cache.put(entry("src/auth", &names(30, "f"), &[]));
        let mut moved: Vec<String> = names(10, "f");
        moved.extend(names(20, "g"));
        let f = cache.freshness(
            &lp("src/auth"),
            &Sketch::build(&moved),
            &[],
            "glm-5.3-flash",
            1,
            DEFAULT_DRIFT_THRESHOLD_PERMILLE,
        );
        let drift = f.drift().expect("stale");
        assert_eq!(drift.cause, DriftCause::Names);
        assert!(
            drift.permille >= DEFAULT_DRIFT_THRESHOLD_PERMILLE,
            "{drift:?}"
        );
        // Ten of thirty names survive, twenty are new: 10 shared of 50 union.
        assert!((drift.fraction() - 0.80).abs() < 0.01, "{drift:?}");
        assert!(f.is_stale());
    }

    /// The case the operator pushed on: `src/services` grows and splits into
    /// `src/services/auth` and `src/services/billing`. The parent's own file
    /// names barely moved — but the parent gave away the part its description
    /// was about, and two new districts have no description at all.
    #[test]
    fn a_split_is_stale_even_when_the_names_barely_moved() {
        let mut cache = ModelCache::default();
        let files = names(20, "svc");
        cache.put(entry("src/services", &files, &[]));

        let after = cache.freshness(
            &lp("src/services"),
            // The parent kept most of its own direct files.
            &Sketch::build(&files[..18]),
            &[lp("src/services/auth"), lp("src/services/billing")],
            "glm-5.3-flash",
            1,
            DEFAULT_DRIFT_THRESHOLD_PERMILLE,
        );
        let drift = after.drift().expect("a split is stale");
        assert_eq!(drift.cause, DriftCause::Split);
        assert!(
            drift.permille < DEFAULT_DRIFT_THRESHOLD_PERMILLE,
            "the names alone would not have caught this: {drift:?}"
        );
        // And the two new districts are simply missing.
        for child in ["src/services/auth", "src/services/billing"] {
            assert_eq!(
                cache.freshness(
                    &lp(child),
                    &Sketch::build(names(9, "x")),
                    &[],
                    "glm-5.3-flash",
                    1,
                    DEFAULT_DRIFT_THRESHOLD_PERMILLE
                ),
                Freshness::Missing
            );
        }
    }

    #[test]
    fn a_merge_is_stale_too() {
        let mut cache = ModelCache::default();
        let files = names(20, "svc");
        cache.put(entry(
            "src/services",
            &files,
            &["src/services/auth", "src/services/billing"],
        ));
        let after = cache.freshness(
            &lp("src/services"),
            &Sketch::build(&files),
            &[lp("src/services/auth")],
            "glm-5.3-flash",
            1,
            DEFAULT_DRIFT_THRESHOLD_PERMILLE,
        );
        assert_eq!(after.drift().expect("stale").cause, DriftCause::Merge);
    }

    #[test]
    fn changing_the_model_or_the_prompt_is_stale_without_discarding_the_text() {
        let mut cache = ModelCache::default();
        let files = names(10, "f");
        cache.put(entry("src/auth", &files, &[]));
        let sketch = Sketch::build(&files);
        let other_model = cache.freshness(
            &lp("src/auth"),
            &sketch,
            &[],
            "glm-4.6",
            1,
            DEFAULT_DRIFT_THRESHOLD_PERMILLE,
        );
        assert_eq!(
            other_model.drift().expect("stale").cause,
            DriftCause::Recipe
        );
        let other_prompt = cache.freshness(
            &lp("src/auth"),
            &sketch,
            &[],
            "glm-5.3-flash",
            2,
            DEFAULT_DRIFT_THRESHOLD_PERMILLE,
        );
        assert_eq!(
            other_prompt.drift().expect("stale").cause,
            DriftCause::Recipe
        );
        // The paid-for text is still there to fall back on.
        assert!(cache
            .get(&lp("src/auth"))
            .expect("entry")
            .description
            .is_some());
    }

    // -- the file -----------------------------------------------------------

    #[test]
    fn a_cache_round_trips_and_a_bad_file_is_a_cold_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("llm.json");
        let mut cache = ModelCache::default();
        cache.put(entry("src/auth", &names(4, "f"), &[]));
        // `None` is cached: "nothing to say" is an answer.
        let mut silent = entry("src/utils", &names(4, "u"), &[]);
        silent.description = None;
        cache.put(silent);
        assert!(cache.is_dirty());
        cache.write(&path);

        let back = ModelCache::read(&path);
        assert_eq!(back.len(), 2);
        assert!(!back.is_dirty());
        assert!(back
            .get(&lp("src/utils"))
            .expect("entry")
            .description
            .is_none());
        assert_eq!(
            back.get(&lp("src/auth"))
                .and_then(|e| e.description.as_ref())
                .map(|d| d.label.as_str()),
            Some("Payment processing")
        );

        std::fs::write(&path, b"not json").expect("write");
        assert!(ModelCache::read(&path).is_empty());
        assert!(ModelCache::read(&dir.path().join("absent.json")).is_empty());
    }

    #[test]
    fn a_version_bump_discards_every_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("llm.json");
        let file = serde_json::json!({
            "version": MODEL_CACHE_VERSION + 1,
            "entries": [],
        });
        std::fs::write(&path, file.to_string()).expect("write");
        assert!(ModelCache::read(&path).is_empty());
    }

    #[test]
    fn deleted_districts_are_reported_before_they_are_dropped() {
        let mut cache = ModelCache::default();
        cache.put(entry("src/auth", &names(3, "a"), &[]));
        cache.put(entry("src/gone", &names(3, "g"), &[]));
        let live: BTreeSet<LogicalPath> = [lp("src/auth")].into_iter().collect();
        assert_eq!(cache.orphans(&live), [lp("src/gone")]);
        assert_eq!(cache.prune(&live), 1);
        assert_eq!(cache.len(), 1);
        assert!(cache.orphans(&live).is_empty());
        cache.clear();
        assert!(cache.is_empty());
    }
}
