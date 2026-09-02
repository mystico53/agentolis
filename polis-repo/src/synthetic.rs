//! A synthetic repository, for measuring the layout at a scale no fixture has.
//!
//! PRD §13.1 budgets cold start → first frame at **5 000 files**, and PRD §16's
//! fixtures are deliberately tiny so a golden-file diff stays readable. Those two
//! needs are different, and the gap between them is where a layout that looks
//! fine on 86 files and falls apart on 5 000 hides.
//!
//! So this builds a repository with a plausible *shape* rather than a uniform
//! one: a few packages that were there from the start and grew large, a long
//! tail of small directories added late, deep nesting in the parts that got
//! refactored, a 20:1 file-size spread, and a vendored tree that PRD §8 masses.
//!
//! # It is a fixture, not a measurement
//!
//! Nothing here is claimed to be a real repository. It is reproducible, it has
//! the right statistics, and it is the honest way to answer "does this hold at
//! scale" without shipping somebody else's source tree in the test suite.
//!
//! Determinism: one `SplitMix64` stream seeded by the caller. No clock, no
//! `HashMap`, no filesystem.

// A fixture generator is arithmetic over counts and shares; the cast lints fire
// on every line of it and the function is one ordered sequence.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use polis_events::{LogicalPath, WallTime, WorktreeId};

use crate::{FileClass, FileMeta, Language, RepoTree};

/// The generator's own random stream.
///
/// Written out rather than taken from `rand`: a fixture that changes when a
/// dependency releases a patch is not a fixture.
#[derive(Debug, Clone, Copy)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    #[allow(clippy::cast_precision_loss)] // 53 bits into a 53-bit mantissa
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_f64() * n as f64) as usize % n
    }
}

/// Top-level packages: `(name, share of the repository, how early it was born)`.
const TOP: &[(&str, f64, f64)] = &[
    ("core", 0.11, 0.00),
    ("server", 0.13, 0.04),
    ("web", 0.19, 0.10),
    ("mobile", 0.07, 0.46),
    ("services", 0.14, 0.22),
    ("packages", 0.09, 0.30),
    ("infra", 0.04, 0.18),
    ("tools", 0.03, 0.55),
    ("docs", 0.03, 0.12),
    ("tests", 0.06, 0.08),
    ("vendor", 0.08, 0.02),
    ("scripts", 0.02, 0.62),
    ("examples", 0.01, 0.72),
];

/// Directory names near the top of a tree.
const MID: &[&str] = &[
    "src",
    "lib",
    "api",
    "model",
    "view",
    "store",
    "auth",
    "billing",
    "search",
    "graph",
    "queue",
    "cache",
    "codec",
    "render",
    "parser",
    "runtime",
    "config",
    "util",
    "net",
    "db",
    "schema",
    "worker",
    "stream",
    "media",
    "ui",
    "hooks",
    "pages",
    "components",
    "handlers",
    "adapters",
    "domain",
    "ports",
];

/// Directory and file names deeper down.
const LEAF: &[&str] = &[
    "session", "token", "user", "account", "invoice", "ledger", "index", "query", "plan", "shard",
    "router", "server", "client", "codec", "frame", "buffer", "pool", "retry", "limit", "clock",
    "trace", "metric", "event", "state", "reducer", "widget", "panel", "modal", "form", "table",
    "chart", "theme", "layout", "sprite", "loader", "mapper", "policy", "guard", "hasher",
    "signer",
];

/// Extensions, with the language each maps to.
const EXT: &[(&str, Option<Language>)] = &[
    ("rs", Some(Language::Rust)),
    ("ts", Some(Language::TypeScript)),
    ("tsx", Some(Language::Tsx)),
    ("py", Some(Language::Python)),
    ("go", None),
    ("md", None),
    ("json", None),
    ("sql", None),
];

/// Seconds of history the synthetic repository spans: eight years.
///
/// Calibrated, not chosen. The two real repositories this pipeline was measured
/// against carry **12.6 years** (Neovim, 3 918 files) and **21.1 years**
/// (Django, 7 083 files) of history; a 5 000-file repository three years old is
/// not a shape that exists, and it is the one the fixture used to have.
const HISTORY_SECONDS: i64 = 8 * 365 * 24 * 3600;

/// The first commit's timestamp. A fixed constant, never the clock.
const EPOCH_SECONDS: i64 = 1_650_000_000;

/// How steeply the fixture's history is front-loaded.
///
/// # This constant became load-bearing when the age ramp started reading clocks
///
/// [`crate::FileMeta::added_at`] used to be decoration: `polis-layout` ramped
/// PRD §7.1's age gradient on a file's *position* in the growth sequence, so any
/// monotone time assignment produced the same city. It now ramps on the
/// timestamps themselves, and the fixture's time distribution decides what the
/// fixture measures.
///
/// Mapping the generator's birth ordering linearly onto the history — which is
/// what this did — produced a repository that added **0.8 %** of its files in
/// its first year, because the directory birth model drifts later at every level
/// of the tree. Measured on the two real repositories: Neovim **36.3 %** of its
/// surviving files in year one of 12.6, Django **3.4 %** in year one of 21.1.
/// Both are front-loaded — an import, a heavy first year or two, then a long
/// thin tail. The fixture was the exact opposite and would have certified an age
/// ramp that never fired.
///
/// So the birth value is raised to this power before it becomes a date, which
/// pulls the mass of the history toward the founding while leaving the *order*
/// — and every tie in it — untouched. `3.5` puts the fixture's first year at
/// **8.2 %** of its files, between the two measured repositories and nearer the
/// harder one.
///
/// Applied as `t·t·t·√t` rather than `powf(3.5)`: `sqrt` is exactly rounded on
/// every target and `powf` is not, and a fixture that is not byte-reproducible
/// turns PRD §16's determinism gate into a test of the C library.
///
/// It is a *fixture* constant and it must not become a tuning knob. The three
/// measurements it is calibrated against are named above; anyone moving it
/// should move it toward a repository that exists.
const AGE_GAMMA: f64 = 3.5;

/// Seconds in a day. Commits land on whole days.
const DAY_SECONDS: i64 = 24 * 3600;

/// One directory the generator decided to fill.
#[derive(Debug)]
struct Dir {
    path: String,
    birth: f64,
    files: u32,
}

/// Build a synthetic [`RepoTree`] of roughly `total` files.
///
/// The result is byte-identical for a given `(total, seed)` on every machine,
/// which is what makes it usable in a determinism gate rather than only in a
/// benchmark.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn repository(total: usize, seed: u64) -> RepoTree {
    let mut rng = SplitMix64(seed);
    let mut dirs: Vec<Dir> = vec![Dir {
        // The repository root: PRD §8's civic square.
        path: String::new(),
        birth: 0.0,
        files: 9,
    }];

    for (name, share, born) in TOP {
        let target = (total as f64 * share).round() as u32;
        let mut frontier: Vec<(String, f64, u32)> = vec![((*name).to_owned(), *born, 1)];
        let mut placed = 0u32;
        let mut guard = 0u32;
        while placed < target && guard < 20_000 {
            guard += 1;
            let pick = ((rng.next_f64().powf(1.5) * frontier.len() as f64) as usize)
                .min(frontier.len() - 1);
            let (parent, pbirth, depth) = frontier[pick].clone();

            // How many files live directly here: a heavy tail.
            let u = rng.next_f64();
            let here = if u < 0.55 {
                1 + (rng.next_f64() * 5.0) as u32
            } else if u < 0.90 {
                4 + (rng.next_f64() * 16.0) as u32
            } else {
                20 + (rng.next_f64() * 70.0) as u32
            };
            let here = here.min(target.saturating_sub(placed)).max(1);
            let jitter = rng.next_f64() * 0.20;
            let birth = (pbirth + jitter * (1.0 - pbirth)).clamp(0.0, 1.0);
            dirs.push(Dir {
                path: parent.clone(),
                birth,
                files: here,
            });
            placed += here;

            if depth < 6 {
                let kids = if depth <= 1 {
                    2 + (rng.next_f64() * 4.0) as u32
                } else if depth <= 3 {
                    1 + (rng.next_f64() * 3.0) as u32
                } else {
                    (rng.next_f64() * 2.0) as u32
                };
                for _ in 0..kids {
                    let pool = if depth <= 1 { MID } else { LEAF };
                    let n = pool[rng.below(pool.len())];
                    let candidate = format!("{parent}/{n}");
                    if frontier.iter().any(|(p, _, _)| *p == candidate) {
                        continue;
                    }
                    let cb = (birth + rng.next_f64() * 0.30 * (1.0 - birth)).clamp(0.0, 1.0);
                    frontier.push((candidate, cb, depth + 1));
                }
            }
            frontier.remove(pick);
            if frontier.is_empty() {
                frontier.push((parent, pbirth, depth));
            }
        }
    }

    // Materialise files with birth times, then rank to get the growth order.
    let mut raw: Vec<(f64, String, u64, Option<Language>)> = Vec::with_capacity(total);
    for d in &dirs {
        for i in 0..d.files {
            let stem = LEAF[rng.below(LEAF.len())];
            let (ext, language) = EXT[rng.below(EXT.len())];
            let name = if d.path.is_empty() {
                format!("{stem}{i}.{ext}")
            } else {
                format!("{}/{}{}.{}", d.path, stem, i, ext)
            };
            // Files appear over a window that opens at the directory's birth.
            let spread = 0.06 + rng.next_f64() * 0.35;
            let t = (d.birth + rng.next_f64().powf(1.6) * spread).clamp(0.0, 1.0);
            let u = rng.next_f64();
            let size = if u < 0.60 {
                300 + (rng.next_f64() * 4_000.0) as u64
            } else if u < 0.95 {
                4_000 + (rng.next_f64() * 30_000.0) as u64
            } else {
                30_000 + (rng.next_f64() * 220_000.0) as u64
            };
            raw.push((t, name, size, language));
        }
    }
    // `(birth, path)`: the growth order is a property of the repository, and the
    // tie-break makes it independent of the order the directories were filled.
    raw.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    raw.truncate(total);

    let mut files: BTreeMap<LogicalPath, FileMeta> = BTreeMap::new();
    for (index, (t, name, size, language)) in raw.iter().enumerate() {
        let Ok(path) = LogicalPath::new(name) else {
            continue;
        };
        // `t^3.5`, exactly (see [`AGE_GAMMA`]), then snapped to a whole day: a
        // real repository commits in bursts, and files that share a day share a
        // commit time, which is what exercises the age ramp's handling of ties
        // rather than handing it 5 000 distinct instants.
        debug_assert!((AGE_GAMMA - 3.5).abs() < f64::EPSILON);
        let aged = t * t * t * t.sqrt();
        let day = (aged * HISTORY_SECONDS as f64 / DAY_SECONDS as f64) as i64;
        let when = WallTime::from_unix_seconds(EPOCH_SECONDS + day * DAY_SECONDS);
        let stem = name.rsplit('/').next().unwrap_or(name);
        let class = if name.starts_with("vendor/") {
            FileClass::Industrial
        } else if stem.starts_with("index") || stem.starts_with("server0") {
            FileClass::Monument
        } else {
            FileClass::Ordinary
        };
        files.insert(
            path.clone(),
            FileMeta {
                path,
                size_bytes: *size,
                growth_index: u32::try_from(index).unwrap_or(u32::MAX),
                added_at: when,
                last_touched: when,
                class,
                language: *language,
            },
        );
    }

    let root = PathBuf::from("/synthetic");
    let mut worktrees = BTreeMap::new();
    worktrees.insert(WorktreeId::PRIMARY, root.clone());
    RepoTree {
        root,
        files,
        worktrees,
        head: format!("synthetic-{total}-{seed:016x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_size_is_close_to_what_was_asked_for() {
        let tree = repository(5_000, 0xACCE_7107_0000_0001);
        assert!(
            tree.files.len() > 4_000 && tree.files.len() <= 5_000,
            "{} files",
            tree.files.len()
        );
    }

    #[test]
    fn growth_indices_are_a_permutation_of_zero_upwards() {
        let tree = repository(400, 7);
        let mut seen: Vec<u32> = tree.files.values().map(|f| f.growth_index).collect();
        seen.sort_unstable();
        for (i, g) in seen.iter().enumerate() {
            assert_eq!(*g as usize, i, "growth index {g} at position {i}");
        }
    }

    #[test]
    fn it_has_the_shape_a_real_repository_has() {
        let tree = repository(2_000, 3);
        let depths: Vec<usize> = tree.files.keys().map(LogicalPath::depth).collect();
        assert!(depths.iter().copied().max().unwrap_or(0) >= 4, "too flat");
        let sizes: Vec<u64> = tree.files.values().map(|f| f.size_bytes).collect();
        let min = sizes.iter().copied().min().unwrap_or(1);
        let max = sizes.iter().copied().max().unwrap_or(1);
        assert!(max > min * 20, "the size spread is too narrow");
        assert!(
            tree.files
                .values()
                .any(|f| f.class == FileClass::Industrial),
            "no vendored tree, so PRD §8's industrial zone is untested"
        );
        let tops: std::collections::BTreeSet<&str> = tree
            .files
            .keys()
            .filter_map(|p| p.components().next())
            .collect();
        assert!(tops.len() >= 8, "only {} top-level packages", tops.len());
    }

    #[test]
    fn it_is_reproducible() {
        let a = repository(600, 11);
        let b = repository(600, 11);
        assert_eq!(a.files, b.files);
        assert_eq!(a.head, b.head);
        let c = repository(600, 12);
        assert_ne!(a.head, c.head);
        assert_ne!(a.files, c.files, "a different seed produced the same tree");
    }
}
