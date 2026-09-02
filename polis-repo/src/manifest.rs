//! A **real repository**, recorded so CI can lay it out without a clone.
//!
//! # What this exists to stop
//!
//! PRD §16 asks for "fixture repos with pinned git history". The suite had two —
//! `hamlet` and `town`, both written by `tests/make-fixtures.sh` — and one
//! synthetic corpus generator, [`crate::synthetic`]. Every structural number in
//! the M1 acceptance table was asserted against the generated corpus, and the
//! real-repository test asserted four trivial properties.
//!
//! That is the shape of a gate a generator can flatter. A corpus written to
//! exercise the layout has a directory tree, a size distribution and a commit
//! curve chosen by the same people who tuned the layout, and it will keep
//! agreeing with them. The measurements that made the point:
//!
//! | corpus | files | solidity | straight border | longest stroke | age ramp |
//! |---|---:|---:|---:|---:|---|
//! | `synthetic::repository(200)` | 200 | 0.763 | 13.9 % | — | calibrated |
//! | `click` (real, 12 years) | 166 | **0.725** | 11.9 % | 38.0 % | calibrated |
//! | `pytest` (real, 18 years) | 690 | **0.831** | 13.0 % | 62.1 % | calibrated |
//! | this workspace (real, **1 day**) | 108 | **0.916** | 20.3 % | **77.6 %** | relative |
//!
//! The last row is the one the synthetic corpus could never have produced: a
//! repository whose entire history fits in a day. It fails two of the criteria
//! the generated corpus passes comfortably, and no amount of tuning against
//! `synthetic::repository` would ever have shown it.
//!
//! # Why a manifest and not a vendored checkout
//!
//! The layout's input is [`RepoTree`], and a `RepoTree` is a list of
//! `(path, size, class, growth index, added-at, last-touched)`. **None of a
//! repository's content reaches the layout** — only that table — so recording
//! the table records everything the city is a function of. A vendored checkout
//! of a real project would add megabytes of source that the layout never reads,
//! carry that project's licence into this tree, and still need its `.git`
//! directory for the growth order.
//!
//! So a manifest is not an approximation of a real repository *for layout
//! purposes*: it is the whole of it. What it deliberately does **not** capture
//! is the import graph (PRD §9 needs file contents for `tree-sitter`), so a
//! corpus fixture exercises the city and not the streets.
//!
//! # Determinism
//!
//! The format is one line per file in `LogicalPath` order with integer fields,
//! so a manifest round-trips exactly and cannot introduce a float. Reading one
//! touches no clock, no filesystem beyond the file itself, and no `HashMap`.
//!
//! # Capturing one
//!
//! [`capture`] writes what [`load`] reads. `polis-repo`'s
//! `corpus_capture::capture_a_corpus` is the ignored test that runs it against a
//! checkout on disk; see `tests/corpora/README.md` for the pinned commits, and
//! ADR-0080 for why the fixtures are manifests and not vendored checkouts.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use polis_events::{LogicalPath, WallTime, WorktreeId};

use crate::{FileClass, RepoTree};

/// Format version, bumped when a column changes meaning.
///
/// A manifest whose version this build does not know is refused rather than
/// half-read: a corpus fixture that silently loses a column would move every
/// number in the acceptance table and look like a layout regression.
pub const FORMAT: u32 = 1;

/// What a manifest records about the repository as a whole.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CorpusHeader {
    /// Short name, used in test output: `click`, `pytest`.
    pub name: String,
    /// Where the repository came from, so the capture can be reproduced.
    pub origin: String,
    /// The commit the capture was taken at. Pinned — this is what makes the
    /// fixture a *fixture*.
    pub head: String,
    /// `git rev-list --count HEAD` at that commit.
    pub commits: u64,
    /// Whether the walk skipped PRD §8's massed trees.
    pub skip_massed: bool,
}

/// A parsed manifest: the header plus the tree it describes.
#[derive(Debug, Clone)]
pub struct Corpus {
    /// The repository the manifest was captured from.
    pub header: CorpusHeader,
    /// The tree `polis-layout` consumes.
    pub tree: RepoTree,
}

/// Why a manifest could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// The first line was not `polis-corpus <version>`.
    NotAManifest,
    /// A format version this build does not know.
    UnknownFormat(u32),
    /// A line that is neither a header nor a well-formed record.
    BadLine {
        /// One-based line number, so the message points at the file.
        line: usize,
        /// What was wrong with it.
        why: &'static str,
    },
    /// The header's `files` count and the number of records disagree.
    CountMismatch {
        /// What the header claimed.
        declared: usize,
        /// What the body held.
        found: usize,
    },
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAManifest => write!(f, "not a polis corpus manifest"),
            Self::UnknownFormat(v) => {
                write!(f, "manifest format {v}, this build reads {FORMAT}")
            }
            Self::BadLine { line, why } => write!(f, "line {line}: {why}"),
            Self::CountMismatch { declared, found } => write!(
                f,
                "the header declares {declared} files and the body holds {found}"
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

/// One character per [`FileClass`], so the column is fixed-width and greppable.
fn class_tag(class: FileClass) -> char {
    match class {
        FileClass::Ordinary => 'o',
        FileClass::Monument => 'm',
        FileClass::Industrial => 'i',
        FileClass::CivicSquare => 'c',
    }
}

/// Inverse of [`class_tag`].
fn class_of(tag: &str) -> Option<FileClass> {
    Some(match tag {
        "o" => FileClass::Ordinary,
        "m" => FileClass::Monument,
        "i" => FileClass::Industrial,
        "c" => FileClass::CivicSquare,
        _ => return None,
    })
}

/// `-` for "git has never seen this", a Unix second count otherwise.
fn time_field(t: WallTime) -> String {
    if t == WallTime::UNIX_EPOCH {
        "-".to_owned()
    } else {
        t.unix_seconds().to_string()
    }
}

/// Serializes `tree` as a manifest [`load`] can read back exactly.
///
/// Line endings are `\n` on every platform: the file is compared byte for byte
/// by a checked-in test and CRLF would make it a different fixture on Windows.
#[must_use]
pub fn capture(tree: &RepoTree, header: &CorpusHeader) -> String {
    let mut out = String::with_capacity(tree.files.len() * 64 + 256);
    let _ = writeln!(out, "polis-corpus {FORMAT}");
    let _ = writeln!(out, "name\t{}", header.name);
    let _ = writeln!(out, "origin\t{}", header.origin);
    let _ = writeln!(out, "head\t{}", header.head);
    let _ = writeln!(out, "commits\t{}", header.commits);
    let _ = writeln!(out, "skip-massed\t{}", header.skip_massed);
    let _ = writeln!(out, "files\t{}", tree.files.len());
    let _ = writeln!(out, "# added\tlast-touched\tgrowth\tbytes\tclass\tpath");
    for meta in tree.files.values() {
        let growth = if meta.is_tracked() {
            meta.growth_index.to_string()
        } else {
            "-".to_owned()
        };
        let _ = writeln!(
            out,
            "{}\t{}\t{growth}\t{}\t{}\t{}",
            time_field(meta.added_at),
            time_field(meta.last_touched),
            meta.size_bytes,
            class_tag(meta.class),
            meta.path.as_str()
        );
    }
    out
}

/// Parses a manifest written by [`capture`].
///
/// # Errors
///
/// [`ManifestError`] when the text is not a manifest this build understands, or
/// when a record is malformed. A corpus fixture is a gate input: half of one is
/// worse than none, so nothing is skipped and nothing is guessed.
// One record grammar, read in one ordered pass; splitting it into a
// header parser and a record parser hides the fact that they share a line loop.
#[allow(clippy::too_many_lines)]
pub fn load(text: &str) -> Result<Corpus, ManifestError> {
    let mut header = CorpusHeader::default();
    let mut declared: Option<usize> = None;
    let mut files: BTreeMap<LogicalPath, crate::FileMeta> = BTreeMap::new();

    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if i == 0 {
            let version = line
                .strip_prefix("polis-corpus ")
                .ok_or(ManifestError::NotAManifest)?
                .trim()
                .parse::<u32>()
                .map_err(|_| ManifestError::NotAManifest)?;
            if version != FORMAT {
                return Err(ManifestError::UnknownFormat(version));
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split('\t');
        let first = parts.next().unwrap_or_default();
        match first {
            "name" | "origin" | "head" | "commits" | "skip-massed" | "files" => {
                let value = parts.next().unwrap_or_default();
                match first {
                    "name" => value.clone_into(&mut header.name),
                    "origin" => value.clone_into(&mut header.origin),
                    "head" => value.clone_into(&mut header.head),
                    "commits" => {
                        header.commits = value.parse().map_err(|_| ManifestError::BadLine {
                            line: line_no,
                            why: "commits is not a number",
                        })?;
                    }
                    "skip-massed" => header.skip_massed = value == "true",
                    _ => {
                        declared = Some(value.parse().map_err(|_| ManifestError::BadLine {
                            line: line_no,
                            why: "files is not a number",
                        })?);
                    }
                }
                continue;
            }
            _ => {}
        }
        // A record. `path` is last and may itself contain no tab, so the split
        // is exact rather than a `splitn` with a remainder.
        let added = parse_time(first).ok_or(ManifestError::BadLine {
            line: line_no,
            why: "added-at is neither `-` nor a Unix second count",
        })?;
        let touched = parts
            .next()
            .and_then(parse_time)
            .ok_or(ManifestError::BadLine {
                line: line_no,
                why: "last-touched is neither `-` nor a Unix second count",
            })?;
        let growth = parts
            .next()
            .and_then(parse_growth)
            .ok_or(ManifestError::BadLine {
                line: line_no,
                why: "growth is neither `-` nor an index",
            })?;
        let size_bytes =
            parts
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or(ManifestError::BadLine {
                    line: line_no,
                    why: "bytes is not a number",
                })?;
        let class = parts
            .next()
            .and_then(class_of)
            .ok_or(ManifestError::BadLine {
                line: line_no,
                why: "class is not one of o/m/i/c",
            })?;
        let path =
            parts
                .next()
                .and_then(|s| LogicalPath::new(s).ok())
                .ok_or(ManifestError::BadLine {
                    line: line_no,
                    why: "path is not a valid logical path",
                })?;
        if parts.next().is_some() {
            return Err(ManifestError::BadLine {
                line: line_no,
                why: "a record has more columns than the format has",
            });
        }
        let meta = crate::FileMeta {
            language: crate::tree::language_for(&path),
            path: path.clone(),
            size_bytes,
            growth_index: growth,
            added_at: added,
            last_touched: touched,
            class,
        };
        files.insert(path, meta);
    }

    if let Some(declared) = declared {
        if declared != files.len() {
            return Err(ManifestError::CountMismatch {
                declared,
                found: files.len(),
            });
        }
    }

    let mut tree = RepoTree {
        // A manifest describes a repository that is not on this disk. The root
        // is a marker, never opened: nothing downstream of `RepoTree` reads it,
        // and a plausible-looking absolute path would invite something to try.
        root: std::path::PathBuf::from(format!("/corpus/{}", header.name)),
        files,
        worktrees: BTreeMap::new(),
        head: header.head.clone(),
    };
    tree.worktrees
        .insert(WorktreeId::PRIMARY, tree.root.clone());
    Ok(Corpus { header, tree })
}

/// `-` or a Unix second count.
fn parse_time(field: &str) -> Option<WallTime> {
    if field == "-" {
        return Some(WallTime::UNIX_EPOCH);
    }
    field.parse::<i64>().ok().map(WallTime::from_unix_seconds)
}

/// `-` (untracked, [`u32::MAX`]) or a growth index.
fn parse_growth(field: &str) -> Option<u32> {
    if field == "-" {
        return Some(u32::MAX);
    }
    field.parse::<u32>().ok().filter(|g| *g != u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileMeta;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("a valid path")
    }

    fn sample() -> RepoTree {
        let mut tree = RepoTree {
            root: std::path::PathBuf::from("/corpus/sample"),
            files: BTreeMap::new(),
            worktrees: BTreeMap::new(),
            head: "deadbeef".to_owned(),
        };
        for (i, (path, size, class)) in [
            ("src/main.rs", 1_200_u64, FileClass::Monument),
            ("src/util.rs", 340, FileClass::Ordinary),
            ("node_modules/x/index.js", 90_000, FileClass::Industrial),
            ("Cargo.toml", 210, FileClass::CivicSquare),
        ]
        .into_iter()
        .enumerate()
        {
            let path = lp(path);
            let mut meta = FileMeta::untracked(path.clone(), size);
            meta.class = class;
            meta.language = crate::tree::language_for(&path);
            meta.growth_index = u32::try_from(i).expect("fits");
            meta.added_at =
                WallTime::from_unix_seconds(1_000_000 + i64::try_from(i).expect("small") * 86_400);
            meta.last_touched = WallTime::from_unix_seconds(2_000_000);
            tree.files.insert(path, meta);
        }
        tree
    }

    fn header() -> CorpusHeader {
        CorpusHeader {
            name: "sample".to_owned(),
            origin: "https://example.invalid/sample.git".to_owned(),
            head: "deadbeef".to_owned(),
            commits: 7,
            skip_massed: false,
        }
    }

    /// The whole point: what goes in comes back out, field for field.
    #[test]
    fn a_manifest_round_trips_exactly() {
        let tree = sample();
        let text = capture(&tree, &header());
        let back = load(&text).expect("reads");
        assert_eq!(back.header, header());
        assert_eq!(back.tree.files.len(), tree.files.len());
        for (path, meta) in &tree.files {
            let got = back.tree.files.get(path).expect("present");
            assert_eq!(got.size_bytes, meta.size_bytes);
            assert_eq!(got.growth_index, meta.growth_index);
            assert_eq!(got.added_at, meta.added_at);
            assert_eq!(got.last_touched, meta.last_touched);
            assert_eq!(got.class, meta.class);
            assert_eq!(got.language, meta.language);
        }
    }

    /// Capturing what was loaded reproduces the file byte for byte, which is
    /// what lets a checked-in fixture be verified rather than trusted.
    #[test]
    fn capture_is_idempotent() {
        let text = capture(&sample(), &header());
        let back = load(&text).expect("reads");
        assert_eq!(capture(&back.tree, &back.header), text);
    }

    /// An untracked file survives the round trip as untracked, not as growth
    /// index zero at the epoch — which would put it in the old town.
    #[test]
    fn untracked_files_stay_untracked() {
        let mut tree = sample();
        let path = lp("scratch.txt");
        tree.files
            .insert(path.clone(), FileMeta::untracked(path.clone(), 12));
        let back = load(&capture(&tree, &header())).expect("reads");
        let got = &back.tree.files[&path];
        assert!(!got.is_tracked());
        assert_eq!(got.added_at, WallTime::UNIX_EPOCH);
    }

    /// No `\r`, on any platform.
    #[test]
    fn the_format_is_lf_only() {
        assert!(!capture(&sample(), &header()).contains('\r'));
    }

    /// A truncated or mangled manifest is refused, not half-read.
    #[test]
    fn a_broken_manifest_is_an_error() {
        assert_eq!(load("hello").err(), Some(ManifestError::NotAManifest));
        assert_eq!(
            load("polis-corpus 99\n").err(),
            Some(ManifestError::UnknownFormat(99))
        );
        let text = capture(&sample(), &header());
        let short = text.replace("files\t4", "files\t5");
        assert_eq!(
            load(&short).err(),
            Some(ManifestError::CountMismatch {
                declared: 5,
                found: 4
            })
        );
        let broken = text.replace("\t1200\t", "\tnotanumber\t");
        assert!(matches!(
            load(&broken).err(),
            Some(ManifestError::BadLine { .. } | ManifestError::CountMismatch { .. })
        ));
    }

    /// Every [`FileClass`] has a tag and every tag a class: a class added
    /// without a tag would silently load as something else.
    #[test]
    fn every_class_round_trips() {
        for class in [
            FileClass::Ordinary,
            FileClass::Monument,
            FileClass::Industrial,
            FileClass::CivicSquare,
        ] {
            assert_eq!(class_of(&class_tag(class).to_string()), Some(class));
        }
        assert_eq!(class_of("z"), None);
    }
}
