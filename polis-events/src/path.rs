//! [`LogicalPath`] — the layout key (PRD §3 glossary, §7.6).
//!
//! > Key the layout on logical path relative to repo root, with the worktree
//! > prefix stripped. […] Decide this now. Retrofitting it into a layout engine
//! > later is painful.
//!
//! So it is decided here, implemented, and tested, before anything is built on
//! top of it. Four channels feed paths in and every one of them spells them
//! differently:
//!
//! * Transcript tool inputs: backslash 11 606 / forward 1 268 — **both**
//!   (`docs/verified/jsonl-schema.md` §9).
//! * `cwd` on transcript records: backslash 125 050 / 125 050.
//! * `tool_input` on the OTel channel: JSON-escaped Windows separators, which
//!   must be JSON-decoded, not string-sliced, or every separator arrives doubled
//!   (`docs/verified/otlp-receiver.md`).
//! * The filesystem watcher: whatever the OS hands back, including `\\?\`
//!   verbatim prefixes on long Windows paths.
//!
//! # The four decisions this type makes
//!
//! 1. **Forward slashes, always.** One canonical spelling; the display form is
//!    also the map key.
//! 2. **Comparison is ASCII case-insensitive, on every platform.** Not
//!    platform-dependent: PRD §16 runs the golden-layout test on two operating
//!    systems and PRD §7.4 requires byte-identical output, so a key that folds on
//!    Windows and not on Linux would fail that test for a reason unrelated to
//!    layout. ASCII-only, deliberately — Unicode case folding is Unicode-version
//!    dependent and would make the city move when the toolchain moves.
//!    Display case is preserved so "click a building → open in `$EDITOR`"
//!    (PRD §12) still opens the real file.
//! 3. **`..` is resolved lexically, never against the filesystem.** The file may
//!    not exist yet — `PreToolUse` fires *before* the write — so
//!    `fs::canonicalize` is not available. A path that escapes the root is
//!    rejected rather than clamped.
//! 4. **The worktree prefix is stripped by [`PathMapper`], and the worktree
//!    identity is returned alongside**, never folded into the path. PRD §7.6:
//!    getting this wrong "means seven near-identical maps side by side and the
//!    loss of the one thing you most want to see".
//!
//! # Known limitation
//!
//! No Unicode normalisation (NFC/NFD). A file created on macOS as `café` in NFD
//! and referenced on Windows in NFC produces two logical paths. Fixing it needs
//! a dependency whose tables change between releases, which would break PRD
//! §7.4's determinism guarantee — so it is documented rather than papered over.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ids::WorktreeId;

/// Why a path could not be turned into a [`LogicalPath`].
///
/// Every variant is a *drop*, counted alongside dropped events (PRD §4.5). None
/// of them is fatal: a path Polis cannot key simply has no building.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathParseError {
    /// The bytes are not valid UTF-8 (a Windows path containing an unpaired
    /// surrogate, or a Unix path containing arbitrary bytes).
    ///
    /// Deliberately **not** lossily converted: `to_string_lossy` would map two
    /// different files onto one key via `U+FFFD`, which is worse than skipping
    /// them.
    #[error("path is not valid UTF-8")]
    NotUtf8,
    /// [`LogicalPath::new`] was handed an absolute path. Absolute paths must go
    /// through [`PathMapper::to_logical`], which knows the worktree roots.
    #[error("expected a repo-relative path, got an absolute one")]
    Absolute,
    /// [`PathMapper`] was handed a relative path with no `cwd` to resolve it
    /// against.
    #[error("expected an absolute path, got a relative one")]
    Relative,
    /// A Windows drive-relative path such as `C:src\\a.rs`, which means "the
    /// current directory *on drive C*" — a per-process, per-drive piece of state
    /// Polis has no access to.
    #[error("drive-relative paths have no fixed meaning")]
    DriveRelative,
    /// A `\\\\server\\share`-style path with no share component.
    #[error("UNC path is missing a server or share component")]
    IncompleteUnc,
    /// `..` walked above the root.
    #[error("path escapes its root with `..`")]
    EscapesRoot,
    /// The path contains a NUL byte and cannot name a real file.
    #[error("path contains a NUL byte")]
    InteriorNul,
}

/// A path relative to the repository root, worktree prefix stripped,
/// forward-slash separated (PRD §3, "Logical path").
///
/// The repository root itself is the empty path; see [`LogicalPath::root`].
///
/// # Equality
///
/// [`Eq`], [`Ord`] and [`Hash`] all fold ASCII case, so `src/Auth.ts` and
/// `SRC/auth.ts` are the same key, on every platform. [`Display`](fmt::Display)
/// and [`as_str`](Self::as_str) return the original casing.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LogicalPath(Box<str>);

impl LogicalPath {
    /// The repository root — the "civic square" of PRD §8.
    pub fn root() -> Self {
        Self(String::new().into_boxed_str())
    }

    /// Normalises a repo-relative path.
    ///
    /// Accepts either separator and any mixture of them, drops `.` components
    /// and empty components, resolves `..` lexically, and strips a trailing
    /// separator. Rejects absolute paths — use [`PathMapper::to_logical`].
    pub fn new(input: &str) -> Result<Self, PathParseError> {
        if input.contains('\0') {
            return Err(PathParseError::InteriorNul);
        }
        if starts_absolute(input) {
            return Err(PathParseError::Absolute);
        }
        let comps = normalize_components(input)?;
        Ok(Self(comps.join("/").into_boxed_str()))
    }

    /// [`LogicalPath::new`] for an [`OsStr`], failing on non-UTF-8 rather than
    /// substituting replacement characters.
    pub fn from_os_str(input: &OsStr) -> Result<Self, PathParseError> {
        Self::new(input.to_str().ok_or(PathParseError::NotUtf8)?)
    }

    /// The canonical string, original casing preserved. Empty for the root.
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True for the repository root.
    #[inline]
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The path's components, root-first. Empty for the root.
    pub fn components(&self) -> impl Iterator<Item = &str> + '_ {
        self.0.split('/').filter(|c| !c.is_empty())
    }

    /// Number of components. `0` for the root, `2` for `src/auth.ts`.
    ///
    /// PRD §6.2 gates territory emission on `depth(A) >= 2`, and PRD §7.2 biases
    /// the terrain field by directory depth.
    pub fn depth(&self) -> usize {
        self.components().count()
    }

    /// The containing directory — the **district** of PRD §3. `None` at the root.
    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        Some(match self.0.rsplit_once('/') {
            Some((head, _)) => Self(head.into()),
            None => Self::root(),
        })
    }

    /// The final component. `None` at the root.
    pub fn file_name(&self) -> Option<&str> {
        if self.is_root() {
            None
        } else {
            Some(self.0.rsplit_once('/').map_or(&*self.0, |(_, tail)| tail))
        }
    }

    /// The extension, without the dot, using the same rule as
    /// [`Path::extension`]: a leading dot is part of the name, not an extension.
    ///
    /// Drives the tree-sitter grammar choice in `polis-repo` (PRD §9).
    pub fn extension(&self) -> Option<&str> {
        let name = self.file_name()?;
        let (stem, ext) = name.rsplit_once('.')?;
        if stem.is_empty() {
            None
        } else {
            Some(ext)
        }
    }

    /// True when `prefix` is this path or an ancestor directory of it.
    ///
    /// Component-aware and case-insensitive, so `src/authority.ts` does **not**
    /// start with `src/auth`. This is the containment test PRD §6.2's
    /// lowest-common-ancestor rule needs.
    pub fn starts_with(&self, prefix: &Self) -> bool {
        if prefix.is_root() {
            return true;
        }
        if self.0.len() < prefix.0.len() {
            return false;
        }
        // Byte slices, never `str::split_at`. `prefix.0.len()` is a *byte*
        // length taken from a different string, so it need not land on a char
        // boundary of `self` — `lp("日本").starts_with(&lp("ab"))` split a
        // multi-byte character and panicked. Paths arrive from four untrusted
        // channels; a panic here takes the ingest thread down.
        let (head, tail) = self.0.as_bytes().split_at(prefix.0.len());
        head.eq_ignore_ascii_case(prefix.0.as_bytes()) && (tail.is_empty() || tail[0] == b'/')
    }

    /// Appends a relative path, normalising it the same way [`Self::new`] does.
    pub fn join(&self, rel: &str) -> Result<Self, PathParseError> {
        if self.is_root() {
            return Self::new(rel);
        }
        let mut joined = self.0.to_string();
        joined.push('/');
        joined.push_str(rel);
        Self::new(&joined)
    }

    /// The lowest common ancestor of two paths (PRD §6.2).
    ///
    /// The territory rule trims the lightest 20% of observations *before*
    /// calling this: untrimmed, "one stray read in `docs/` promotes the
    /// territory to repo root and claims the entire city".
    #[must_use]
    pub fn common_ancestor(&self, other: &Self) -> Self {
        let mut out = String::new();
        for (a, b) in self.components().zip(other.components()) {
            if !a.eq_ignore_ascii_case(b) {
                break;
            }
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(a);
        }
        Self(out.into_boxed_str())
    }

    /// A stable 64-bit seed derived from the path (PRD §7.4).
    ///
    /// > Every random draw is seeded from a hash of the logical path. Never from
    /// > wall clock, never from a global RNG, never from iteration order of a
    /// > `HashMap`.
    ///
    /// This is FNV-1a over the case-folded bytes, written out rather than
    /// delegated to [`std::hash::DefaultHasher`], because `DefaultHasher`'s
    /// algorithm is explicitly not guaranteed stable across Rust releases — and
    /// PRD §7.4 requires the same repo to produce the same city "on every launch
    /// and on every machine", which includes machines on a different toolchain.
    pub fn layout_seed(&self) -> u64 {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut h = OFFSET;
        for b in self.0.bytes() {
            h ^= u64::from(b.to_ascii_lowercase());
            h = h.wrapping_mul(PRIME);
        }
        h
    }
}

impl fmt::Debug for LogicalPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LogicalPath({:?})", &*self.0)
    }
}

impl fmt::Display for LogicalPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq for LogicalPath {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }
}

impl Eq for LogicalPath {}

impl PartialOrd for LogicalPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LogicalPath {
    /// Byte-wise over ASCII-folded bytes. Multi-byte UTF-8 sequences are all
    /// `>= 0x80` and unaffected by `to_ascii_lowercase`, so this stays a total
    /// order consistent with [`PartialEq`], and identical on every platform —
    /// which is what makes `BTreeMap` iteration order safe to feed into the
    /// layout (PRD §7.4).
    fn cmp(&self, other: &Self) -> Ordering {
        let (a, b) = (self.0.as_bytes(), other.0.as_bytes());
        for i in 0..a.len().min(b.len()) {
            let (x, y) = (a[i].to_ascii_lowercase(), b[i].to_ascii_lowercase());
            if x != y {
                return x.cmp(&y);
            }
        }
        a.len().cmp(&b.len())
    }
}

impl Hash for LogicalPath {
    /// Must agree with [`PartialEq`], so it hashes the folded bytes. The
    /// trailing sentinel keeps `["ab", "c"]` from colliding with `["a", "bc"]`
    /// in any composite key.
    fn hash<H: Hasher>(&self, state: &mut H) {
        for b in self.0.bytes() {
            state.write_u8(b.to_ascii_lowercase());
        }
        state.write_u8(0xff);
    }
}

impl FromStr for LogicalPath {
    type Err = PathParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl Default for LogicalPath {
    fn default() -> Self {
        Self::root()
    }
}

/// Maps absolute filesystem paths onto [`LogicalPath`] plus a [`WorktreeId`]
/// (PRD §7.6).
///
/// `/repo-wt-3/src/auth.ts` and `/repo-wt-7/src/auth.ts` are the same logical
/// file in two physical places; both map to `src/auth.ts` with different
/// worktree ids. That is what lets the city show "two agents editing the same
/// file on different branches" instead of seven near-identical maps.
///
/// Roots are matched **longest first**, so a worktree created *inside* the
/// repository (a common habit) resolves to the worktree rather than to the
/// repository root.
#[derive(Debug, Clone, Default)]
pub struct PathMapper {
    /// Sorted by component count descending; ties broken by id for determinism.
    roots: Vec<RootEntry>,
}

#[derive(Debug, Clone)]
struct RootEntry {
    id: WorktreeId,
    root: AbsPath,
    display: String,
}

impl PathMapper {
    /// Creates a mapper for a repository's primary checkout.
    pub fn new(repo_root: &Path) -> Result<Self, PathParseError> {
        let mut m = Self::default();
        m.add_root(WorktreeId::PRIMARY, repo_root)?;
        Ok(m)
    }

    /// Registers an additional `git worktree` checkout.
    ///
    /// Discovered from `SessionStart`'s `cwd`, from
    /// [`CwdChanged`](crate::EventKind::CwdChanged), or from `git worktree list`
    /// — never from a `WorktreeCreate` hook, which Polis must not register
    /// (`docs/verified/hooks-schema.md` §9.1).
    pub fn add_worktree(&mut self, id: WorktreeId, root: &Path) -> Result<(), PathParseError> {
        self.add_root(id, root)
    }

    fn add_root(&mut self, id: WorktreeId, root: &Path) -> Result<(), PathParseError> {
        let text = root.to_str().ok_or(PathParseError::NotUtf8)?;
        let abs = AbsPath::parse(text)?;
        let display = abs.to_display();
        self.roots.retain(|r| r.id != id);
        self.roots.push(RootEntry {
            id,
            root: abs,
            display,
        });
        self.roots.sort_by(|a, b| {
            b.root
                .comps
                .len()
                .cmp(&a.root.comps.len())
                .then(a.id.cmp(&b.id))
        });
        Ok(())
    }

    /// Every registered root, longest first.
    pub fn roots(&self) -> impl Iterator<Item = (WorktreeId, &str)> + '_ {
        self.roots.iter().map(|r| (r.id, r.display.as_str()))
    }

    /// The normalised, forward-slash root directory of a checkout.
    pub fn worktree_root(&self, id: WorktreeId) -> Option<&str> {
        self.roots
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.display.as_str())
    }

    /// Maps an absolute path onto its worktree and logical path.
    ///
    /// Returns `None` when the path is outside every registered root — a
    /// `~/.claude` transcript, a temp file, `C:\Windows\...`. That is a normal
    /// condition, not an error: those files have no building.
    pub fn to_logical(&self, path: &Path) -> Option<(WorktreeId, LogicalPath)> {
        self.to_logical_str(path.to_str()?)
    }

    /// [`Self::to_logical`] for a path that arrived as a string — which is how
    /// every channel actually delivers them.
    pub fn to_logical_str(&self, path: &str) -> Option<(WorktreeId, LogicalPath)> {
        let abs = AbsPath::parse(path).ok()?;
        for entry in &self.roots {
            if let Some(rel) = abs.strip_root(&entry.root) {
                return Some((entry.id, LogicalPath(rel.join("/").into_boxed_str())));
            }
        }
        None
    }

    /// Maps a path that may be relative, resolving it against `cwd` first.
    ///
    /// Tool inputs carry both forms; `Grep`'s `path` is frequently relative
    /// while `Read`'s `file_path` is usually absolute.
    pub fn resolve(&self, cwd: Option<&Path>, path: &str) -> Option<(WorktreeId, LogicalPath)> {
        if starts_absolute(path) || AbsPath::parse(path).is_ok() {
            return self.to_logical_str(path);
        }
        let cwd = cwd?.to_str()?;
        let mut joined = String::with_capacity(cwd.len() + 1 + path.len());
        joined.push_str(cwd);
        joined.push('/');
        joined.push_str(path);
        self.to_logical_str(&joined)
    }
}

/// A parsed absolute path: a root prefix plus normalised components.
///
/// Kept as a component vector rather than a string so that prefix matching is a
/// component comparison and cannot produce the classic `\repo` / `\repo-backup`
/// false positive.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AbsPath {
    /// `""` for a Unix root, `"C:"` for a drive, `"//server/share"` for UNC.
    prefix: String,
    comps: Vec<String>,
}

impl AbsPath {
    fn parse(input: &str) -> Result<Self, PathParseError> {
        if input.contains('\0') {
            return Err(PathParseError::InteriorNul);
        }
        let slashed = input.replace('\\', "/");
        let mut s = slashed.as_str();

        // `\\?\C:\x` and `\\?\UNC\server\share\x` — the verbatim prefixes Windows
        // hands back for long paths. A 200-char-capped project directory yields a
        // 282-char transcript path on a machine with LongPathsEnabled=0
        // (docs/verified/jsonl-schema.md §1), so these are not hypothetical.
        if let Some(rest) = s.strip_prefix("//?/") {
            // Compared as *bytes*. `rest[..4]` is a `str` index and byte 4 lands
            // inside a multi-byte character for the very ordinary
            // `\\?\C:\日本\a.rs` — verbatim prefixes and non-ASCII directory
            // names co-occur constantly, and that panicked.
            if rest
                .as_bytes()
                .get(..4)
                .is_some_and(|p| p.eq_ignore_ascii_case(b"UNC/"))
            {
                // Re-form as a plain UNC path so one branch handles both.
                // `rest[4..]` is safe: the first four bytes are ASCII.
                return Self::parse_from_unc(&rest[4..]);
            }
            s = rest;
        }

        let (prefix, rest) = if let Some(rest) = s.strip_prefix("//") {
            return Self::parse_from_unc(rest);
        } else if let Some(rest) = s.strip_prefix('/') {
            (String::new(), rest)
        } else if let Some(drive) = drive_prefix(s) {
            let rest = &s[2..];
            if rest.is_empty() {
                (drive, "")
            } else if let Some(stripped) = rest.strip_prefix('/') {
                (drive, stripped)
            } else {
                // `C:src\a.rs` means "relative to the current directory on C:",
                // which is per-process state Polis cannot see.
                return Err(PathParseError::DriveRelative);
            }
        } else {
            return Err(PathParseError::Relative);
        };

        Ok(Self {
            prefix,
            comps: normalize_components(rest)?,
        })
    }

    /// `server/share/rest…`, already slash-normalised and with the leading `//`
    /// or `\\?\UNC\` removed.
    fn parse_from_unc(rest: &str) -> Result<Self, PathParseError> {
        let mut it = rest.splitn(3, '/');
        let server = it.next().unwrap_or_default();
        let share = it.next().unwrap_or_default();
        if server.is_empty() || share.is_empty() {
            return Err(PathParseError::IncompleteUnc);
        }
        let tail = it.next().unwrap_or_default();
        Ok(Self {
            prefix: format!("//{server}/{share}"),
            comps: normalize_components(tail)?,
        })
    }

    /// Returns the components below `root`, or `None` if this path is outside it.
    fn strip_root(&self, root: &Self) -> Option<Vec<&str>> {
        if !self.prefix.eq_ignore_ascii_case(&root.prefix) || self.comps.len() < root.comps.len() {
            return None;
        }
        for (a, b) in self.comps.iter().zip(&root.comps) {
            if !a.eq_ignore_ascii_case(b) {
                return None;
            }
        }
        Some(
            self.comps[root.comps.len()..]
                .iter()
                .map(String::as_str)
                .collect(),
        )
    }

    fn to_display(&self) -> String {
        if self.comps.is_empty() {
            if self.prefix.is_empty() {
                "/".to_owned()
            } else {
                self.prefix.clone()
            }
        } else {
            format!("{}/{}", self.prefix, self.comps.join("/"))
        }
    }
}

/// `C:` for `C:/x` or `C:`; `None` otherwise.
fn drive_prefix(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        Some(s[..2].to_owned())
    } else {
        None
    }
}

/// True for anything that names a filesystem root: `/x`, `\\x`, `C:\\x`, `//srv/s`.
fn starts_absolute(s: &str) -> bool {
    s.starts_with('/') || s.starts_with('\\') || drive_prefix(s).is_some()
}

/// Splits on either separator, drops `.` and empty components, resolves `..`.
fn normalize_components(input: &str) -> Result<Vec<String>, PathParseError> {
    let mut out: Vec<String> = Vec::new();
    for comp in input.split(['/', '\\']) {
        match comp {
            "" | "." => {}
            ".." => {
                if out.pop().is_none() {
                    return Err(PathParseError::EscapesRoot);
                }
            }
            other => out.push(other.to_owned()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use super::*;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).unwrap_or_else(|e| panic!("{s:?} should parse: {e}"))
    }

    // ---- normalisation ----------------------------------------------------

    #[test]
    fn backslashes_become_forward_slashes() {
        assert_eq!(lp(r"src\auth\mod.rs").as_str(), "src/auth/mod.rs");
        // Mixed, which is what the corpus actually contains.
        assert_eq!(lp(r"src/auth\mod.rs").as_str(), "src/auth/mod.rs");
        assert_eq!(lp(r"src\auth/mod.rs").as_str(), "src/auth/mod.rs");
    }

    #[test]
    fn redundant_separators_and_dots_are_dropped() {
        assert_eq!(lp("src//auth///mod.rs").as_str(), "src/auth/mod.rs");
        assert_eq!(lp("./src/./auth/mod.rs").as_str(), "src/auth/mod.rs");
        assert_eq!(lp("src/auth/").as_str(), "src/auth");
        assert_eq!(lp(r"src\auth\\").as_str(), "src/auth");
    }

    #[test]
    fn dotdot_resolves_lexically() {
        assert_eq!(lp("src/auth/../db/pool.rs").as_str(), "src/db/pool.rs");
        assert_eq!(lp("src/auth/..").as_str(), "src");
        assert_eq!(lp("a/b/c/../../d").as_str(), "a/d");
        assert_eq!(
            LogicalPath::new("../outside"),
            Err(PathParseError::EscapesRoot)
        );
        assert_eq!(
            LogicalPath::new("src/../../x"),
            Err(PathParseError::EscapesRoot)
        );
    }

    #[test]
    fn root_has_many_spellings_and_one_value() {
        for s in ["", ".", "./", "./.", "a/.."] {
            assert!(LogicalPath::new(s).unwrap().is_root(), "{s:?}");
        }
        assert_eq!(LogicalPath::root().as_str(), "");
        assert_eq!(LogicalPath::root().depth(), 0);
        assert_eq!(LogicalPath::root().parent(), None);
        assert_eq!(LogicalPath::root().file_name(), None);
        assert_eq!(LogicalPath::default(), LogicalPath::root());
    }

    #[test]
    fn absolute_paths_are_refused_here() {
        for s in [
            r"C:\repo\src",
            "/repo/src",
            r"\\server\share\x",
            r"\repo",
            "c:/x",
        ] {
            assert_eq!(LogicalPath::new(s), Err(PathParseError::Absolute), "{s:?}");
        }
    }

    #[test]
    fn nul_bytes_are_refused() {
        assert_eq!(
            LogicalPath::new("src/a\0b.rs"),
            Err(PathParseError::InteriorNul)
        );
    }

    // ---- case insensitivity ----------------------------------------------

    #[test]
    fn comparison_folds_ascii_case_but_display_does_not() {
        let a = lp("src/Auth.ts");
        let b = lp("SRC/auth.TS");
        assert_eq!(a, b);
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert_eq!(
            a.as_str(),
            "src/Auth.ts",
            "display case must survive for $EDITOR"
        );
        assert_eq!(b.as_str(), "SRC/auth.TS");
        assert_eq!(a.layout_seed(), b.layout_seed(), "one file, one building");
    }

    #[test]
    fn one_key_per_file_in_both_map_flavours() {
        let mut h: HashMap<LogicalPath, u32> = HashMap::new();
        h.insert(lp("src/Auth.ts"), 1);
        h.insert(lp("SRC/auth.ts"), 2);
        assert_eq!(h.len(), 1, "case variants must not create two buildings");
        assert_eq!(h[&lp("src/AUTH.TS")], 2);

        // PRD §7.4 mandates BTreeMap wherever iteration order reaches layout.
        let mut b: BTreeMap<LogicalPath, u32> = BTreeMap::new();
        b.insert(lp("src/Auth.ts"), 1);
        b.insert(lp("SRC/auth.ts"), 2);
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn ordering_is_total_and_platform_independent() {
        let mut v = [
            lp("src/b.rs"),
            lp("SRC/a.rs"),
            lp("docs/x.md"),
            LogicalPath::root(),
        ];
        v.sort();
        let got: Vec<&str> = v.iter().map(LogicalPath::as_str).collect();
        assert_eq!(got, ["", "docs/x.md", "SRC/a.rs", "src/b.rs"]);
        // Consistency with Eq: equal elements compare Equal, not by raw bytes.
        assert_eq!(lp("A").cmp(&lp("a")), Ordering::Equal);
        assert_eq!(lp("a").cmp(&lp("ab")), Ordering::Less);
    }

    #[test]
    fn non_ascii_case_is_deliberately_not_folded() {
        // Folding these would need Unicode tables whose contents change between
        // releases, which PRD §7.4 forbids. Documented, tested, and stable.
        assert_ne!(lp("src/Ä.rs"), lp("src/ä.rs"));
        assert_eq!(lp("src/日本語/テスト.rs").depth(), 3);
        assert_eq!(lp("src/café.rs").file_name(), Some("café.rs"));
    }

    // ---- accessors --------------------------------------------------------

    #[test]
    fn accessors_behave_like_paths() {
        let p = lp("src/auth/mod.rs");
        assert_eq!(p.depth(), 3);
        assert_eq!(p.file_name(), Some("mod.rs"));
        assert_eq!(p.extension(), Some("rs"));
        assert_eq!(p.parent().unwrap().as_str(), "src/auth");
        assert_eq!(p.parent().unwrap().parent().unwrap().as_str(), "src");
        assert_eq!(
            p.parent().unwrap().parent().unwrap().parent(),
            Some(LogicalPath::root())
        );
        assert_eq!(
            p.components().collect::<Vec<_>>(),
            ["src", "auth", "mod.rs"]
        );

        assert_eq!(lp("README").extension(), None);
        assert_eq!(
            lp(".gitignore").extension(),
            None,
            "a dotfile is a name, not an extension"
        );
        assert_eq!(lp("a.tar.gz").extension(), Some("gz"));
    }

    #[test]
    fn starts_with_respects_component_boundaries() {
        let p = lp("src/auth/mod.rs");
        assert!(p.starts_with(&LogicalPath::root()));
        assert!(p.starts_with(&lp("src")));
        assert!(p.starts_with(&lp("SRC/AUTH")));
        assert!(p.starts_with(&p));
        assert!(!p.starts_with(&lp("sr")));
        assert!(!lp("src/authority.ts").starts_with(&lp("src/auth")));
        assert!(!lp("src").starts_with(&lp("src/auth")));
    }

    #[test]
    fn join_normalises_what_it_appends() {
        assert_eq!(
            lp("src").join(r"auth\mod.rs").unwrap().as_str(),
            "src/auth/mod.rs"
        );
        assert_eq!(
            lp("src/auth").join("../db.rs").unwrap().as_str(),
            "src/db.rs"
        );
        assert_eq!(LogicalPath::root().join("a/b").unwrap().as_str(), "a/b");
        assert_eq!(lp("src").join("../../x"), Err(PathParseError::EscapesRoot));
    }

    #[test]
    fn common_ancestor_is_the_lca_prd_6_2_needs() {
        assert_eq!(
            lp("src/auth/a.rs")
                .common_ancestor(&lp("src/auth/b.rs"))
                .as_str(),
            "src/auth"
        );
        assert_eq!(
            lp("src/auth/a.rs")
                .common_ancestor(&lp("SRC/db/b.rs"))
                .as_str(),
            "src"
        );
        assert!(lp("src/a.rs").common_ancestor(&lp("docs/b.md")).is_root());
        assert_eq!(lp("src/a.rs").common_ancestor(&lp("src")).as_str(), "src");
    }

    #[test]
    fn layout_seed_is_pinned() {
        // These values are a contract, not an observation: PRD §7.4 requires the
        // same city on every machine and every toolchain. If this test fails,
        // every golden layout file in the repo is invalidated on purpose.
        assert_eq!(LogicalPath::root().layout_seed(), 0xcbf2_9ce4_8422_2325);
        assert_eq!(lp("src/auth/mod.rs").layout_seed(), 0xe38c_28c3_8ed8_7514);
        assert_eq!(lp("SRC/AUTH/MOD.RS").layout_seed(), 0xe38c_28c3_8ed8_7514);
        assert_ne!(lp("src/a.rs").layout_seed(), lp("src/b.rs").layout_seed());
    }

    // ---- non-UTF-8 --------------------------------------------------------

    #[cfg(windows)]
    #[test]
    fn a_lone_surrogate_is_rejected_not_mangled() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        // 0xD800 unpaired: legal in a Windows filename, not encodable as UTF-8.
        let os = OsString::from_wide(&[0x0073, 0x0072, 0x0063, 0x005c, 0xD800]);
        assert_eq!(LogicalPath::from_os_str(&os), Err(PathParseError::NotUtf8));
    }

    #[cfg(unix)]
    #[test]
    fn arbitrary_bytes_are_rejected_not_mangled() {
        use std::os::unix::ffi::OsStrExt;
        let os = OsStr::from_bytes(b"src/\xff\xfe.rs");
        assert_eq!(LogicalPath::from_os_str(os), Err(PathParseError::NotUtf8));
    }

    #[test]
    fn valid_utf8_os_strings_pass_through() {
        assert_eq!(
            LogicalPath::from_os_str(OsStr::new(r"src\a.rs"))
                .unwrap()
                .as_str(),
            "src/a.rs"
        );
    }

    // ---- PathMapper -------------------------------------------------------

    fn mapper() -> PathMapper {
        PathMapper::new(Path::new(r"C:\coding\agentolis")).unwrap()
    }

    #[test]
    fn strips_a_windows_repo_root_case_insensitively() {
        let m = mapper();
        for input in [
            r"C:\coding\agentolis\src\auth\mod.rs",
            r"c:\CODING\Agentolis\src\auth\mod.rs",
            "C:/coding/agentolis/src/auth/mod.rs",
            r"C:\coding\agentolis\.\src\auth\mod.rs",
            r"C:\coding\agentolis\src\db\..\auth\mod.rs",
        ] {
            let (wt, p) = m.to_logical_str(input).unwrap_or_else(|| panic!("{input}"));
            assert_eq!(wt, WorktreeId::PRIMARY);
            assert_eq!(p.as_str(), "src/auth/mod.rs", "{input}");
        }
        assert!(m
            .to_logical_str(r"C:\coding\agentolis")
            .unwrap()
            .1
            .is_root());
    }

    #[test]
    fn strips_a_verbatim_long_path_prefix() {
        // std hands these back for >MAX_PATH paths, and the 282-char transcript
        // path measured in docs/verified/jsonl-schema.md §1 is exactly that case.
        let m = mapper();
        let (_, p) = m
            .to_logical_str(r"\\?\C:\coding\agentolis\src\a.rs")
            .unwrap();
        assert_eq!(p.as_str(), "src/a.rs");
    }

    #[test]
    fn handles_unc_roots_including_the_verbatim_spelling() {
        let mut m = PathMapper::new(Path::new(r"\\build01\repos\polis")).unwrap();
        m.add_worktree(WorktreeId(4), Path::new(r"\\build01\repos\polis-wt-4"))
            .unwrap();

        let (wt, p) = m.to_logical_str(r"\\build01\repos\polis\src\a.rs").unwrap();
        assert_eq!((wt, p.as_str()), (WorktreeId::PRIMARY, "src/a.rs"));

        let (wt, p) = m
            .to_logical_str(r"\\?\UNC\BUILD01\repos\polis-wt-4\src\a.rs")
            .unwrap();
        assert_eq!((wt, p.as_str()), (WorktreeId(4), "src/a.rs"));

        // A different share is not the same repo.
        assert!(m
            .to_logical_str(r"\\build01\other\polis\src\a.rs")
            .is_none());
        assert_eq!(
            m.worktree_root(WorktreeId::PRIMARY),
            Some("//build01/repos/polis")
        );
    }

    #[test]
    fn unix_roots_work_too() {
        let mut m = PathMapper::new(Path::new("/home/op/repo")).unwrap();
        m.add_worktree(WorktreeId(3), Path::new("/home/op/repo-wt-3"))
            .unwrap();
        let (wt, p) = m.to_logical_str("/home/op/repo/src/auth.ts").unwrap();
        assert_eq!((wt, p.as_str()), (WorktreeId::PRIMARY, "src/auth.ts"));
        let (wt, q) = m.to_logical_str("/home/op/repo-wt-3/src/auth.ts").unwrap();
        assert_eq!(wt, WorktreeId(3));
        // PRD §7.6: the same logical file in two physical places.
        assert_eq!(p, q);
        assert_eq!(m.worktree_root(WorktreeId::PRIMARY), Some("/home/op/repo"));
    }

    #[test]
    fn a_sibling_root_with_a_shared_prefix_is_not_a_match() {
        // The classic bug: `/home/op/repo-backup` starts with `/home/op/repo`
        // as a string but is a different tree.
        let m = PathMapper::new(Path::new("/home/op/repo")).unwrap();
        assert!(m.to_logical_str("/home/op/repo-backup/src/a.rs").is_none());
        assert!(m.to_logical_str("/home/op/repository/src/a.rs").is_none());
    }

    #[test]
    fn the_longest_matching_root_wins() {
        // A worktree created inside the repository is a common habit.
        let mut m = PathMapper::new(Path::new("/home/op/repo")).unwrap();
        m.add_worktree(WorktreeId(7), Path::new("/home/op/repo/.worktrees/wt7"))
            .unwrap();
        let (wt, p) = m
            .to_logical_str("/home/op/repo/.worktrees/wt7/src/a.rs")
            .unwrap();
        assert_eq!((wt, p.as_str()), (WorktreeId(7), "src/a.rs"));
        let (wt, p) = m.to_logical_str("/home/op/repo/src/a.rs").unwrap();
        assert_eq!((wt, p.as_str()), (WorktreeId::PRIMARY, "src/a.rs"));
    }

    #[test]
    fn paths_outside_every_root_are_none_not_an_error() {
        let m = mapper();
        for outside in [
            r"C:\Users\konka\.claude\projects\x\y.jsonl",
            r"D:\coding\agentolis\src\a.rs",
            r"C:\Windows\System32\drivers\etc\hosts",
            "/home/op/repo/src/a.rs",
        ] {
            assert!(m.to_logical_str(outside).is_none(), "{outside}");
        }
    }

    #[test]
    fn malformed_absolute_paths_are_none_not_a_panic() {
        let m = mapper();
        for bad in [
            r"C:src\a.rs",
            r"\\server",
            r"\\",
            "",
            "relative/a.rs",
            "C:",
            "::::",
        ] {
            assert!(m.to_logical_str(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn resolve_joins_relative_paths_against_cwd() {
        let m = mapper();
        let cwd = Path::new(r"C:\coding\agentolis\src");
        let (_, p) = m.resolve(Some(cwd), "auth/mod.rs").unwrap();
        assert_eq!(p.as_str(), "src/auth/mod.rs");
        let (_, p) = m.resolve(Some(cwd), "../docs/PRD.md").unwrap();
        assert_eq!(p.as_str(), "docs/PRD.md");
        // Absolute input ignores cwd entirely.
        let (_, p) = m
            .resolve(Some(cwd), r"C:\coding\agentolis\Cargo.toml")
            .unwrap();
        assert_eq!(p.as_str(), "Cargo.toml");
        // Relative with no cwd is unresolvable, not a guess.
        assert!(m.resolve(None, "auth/mod.rs").is_none());
    }

    #[test]
    fn re_adding_a_worktree_id_replaces_it() {
        let mut m = PathMapper::new(Path::new("/home/op/repo")).unwrap();
        m.add_worktree(WorktreeId(1), Path::new("/tmp/a")).unwrap();
        m.add_worktree(WorktreeId(1), Path::new("/tmp/b")).unwrap();
        assert_eq!(m.roots().count(), 2);
        assert_eq!(m.worktree_root(WorktreeId(1)), Some("/tmp/b"));
        assert!(m.to_logical_str("/tmp/a/x.rs").is_none());
    }

    // ---- non-ASCII must never reach a byte-index slice -------------------
    //
    // Every case below crashed the ingest thread before the fix that followed
    // it: `str` indexing panics on a non-char-boundary index, and both call
    // sites derived the index from a *byte* length. Paths arrive from four
    // untrusted channels, so a panic here is a denial of service on a
    // Japanese, German or emoji-named directory.

    #[test]
    fn starts_with_does_not_panic_on_a_multibyte_boundary() {
        // `prefix.0.len()` is 2 bytes; byte 2 of "日本" is inside the first
        // character. The old `str::split_at` panicked here.
        assert!(!lp("日本").starts_with(&lp("ab")));
        assert!(!lp("日本/a.rs").starts_with(&lp("日")));
        assert!(!lp("ä.rs").starts_with(&lp("a")));
        assert!(!lp("café/x.rs").starts_with(&lp("caf")));
        // …and the true cases still hold with non-ASCII components.
        assert!(lp("日本語/src/a.rs").starts_with(&lp("日本語")));
        assert!(lp("日本語/src/a.rs").starts_with(&lp("日本語/src")));
        assert!(!lp("日本語/src/a.rs").starts_with(&lp("日本語x")));
    }

    #[test]
    fn common_ancestor_and_starts_with_agree_on_non_ascii() {
        let a = lp("src/日本語/a.rs");
        let b = lp("src/日本語/b.rs");
        let lca = a.common_ancestor(&b);
        assert_eq!(lca.as_str(), "src/日本語");
        assert!(a.starts_with(&lca) && b.starts_with(&lca));
    }

    #[test]
    fn a_verbatim_prefix_over_a_non_ascii_path_does_not_panic() {
        // `\\?\C:\日本\a.rs`: byte 4 of "C:/日本/a.rs" is inside `日`. The old
        // `rest[..4]` UNC probe panicked before it ever reached the drive
        // branch. Long paths and non-ASCII directory names co-occur constantly.
        let mut m = PathMapper::new(Path::new(r"C:\coding\日本語")).unwrap();
        let (wt, p) = m
            .to_logical_str(r"\\?\C:\coding\日本語\src\a.rs")
            .expect("verbatim non-ASCII path must map");
        assert_eq!((wt, p.as_str()), (WorktreeId::PRIMARY, "src/a.rs"));

        // The same probe over a path whose first component is non-ASCII.
        assert!(m.to_logical_str(r"\\?\日本語\a.rs").is_none());
        assert!(m.to_logical_str(r"\\?\ä").is_none());
        assert!(m.to_logical_str("//?/").is_none());
        assert!(m.to_logical_str("//?/UN").is_none());

        // And the UNC spelling of the same, still case-insensitive on `UNC`.
        m.add_worktree(WorktreeId(2), Path::new(r"\\srv\share\日本語"))
            .unwrap();
        let (wt, p) = m
            .to_logical_str(r"\\?\unc\SRV\share\日本語\src\a.rs")
            .expect("verbatim UNC over a non-ASCII path must map");
        assert_eq!((wt, p.as_str()), (WorktreeId(2), "src/a.rs"));
    }

    #[test]
    fn non_ascii_survives_every_normalisation_route() {
        assert_eq!(lp(r"src\日本語\テスト.rs").as_str(), "src/日本語/テスト.rs");
        assert_eq!(lp("src/日本語/../ä/b.rs").as_str(), "src/ä/b.rs");
        assert_eq!(lp("src/日本語/").as_str(), "src/日本語");
        assert_eq!(lp("emoji/🏙️/city.rs").depth(), 3);
        assert_eq!(
            lp("src/日本語/a.rs").parent().unwrap().as_str(),
            "src/日本語"
        );
        assert_eq!(lp("src/日本語.rs").extension(), Some("rs"));
        assert_eq!(lp("src/日本語.テスト").extension(), Some("テスト"));
        assert_eq!(
            lp("src").join("日本語/a.rs").unwrap().as_str(),
            "src/日本語/a.rs"
        );
    }

    /// `Ord` must be a strict weak ordering over a mixed ASCII / non-ASCII set,
    /// or `BTreeMap` — which PRD §7.4 mandates wherever iteration order reaches
    /// layout — silently loses entries.
    #[test]
    fn ordering_is_antisymmetric_and_transitive_over_mixed_scripts() {
        let v: Vec<LogicalPath> = [
            "",
            "a",
            "A",
            "ab",
            "a/b",
            "a-b",
            "ä",
            "Ä",
            "日本語",
            "日本",
            "src/Auth.ts",
            "SRC/auth.ts",
            "src/日本語/a.rs",
        ]
        .iter()
        .map(|s| lp(s))
        .collect();
        for a in &v {
            for b in &v {
                assert_eq!(
                    a.cmp(b),
                    b.cmp(a).reverse(),
                    "not antisymmetric: {a:?} vs {b:?}"
                );
                assert_eq!(a == b, a.cmp(b) == Ordering::Equal, "{a:?} vs {b:?}");
                for c in &v {
                    if a.cmp(b) == Ordering::Less && b.cmp(c) == Ordering::Less {
                        assert_eq!(a.cmp(c), Ordering::Less, "{a:?} {b:?} {c:?}");
                    }
                }
            }
        }
    }

    // ---- further mapper edge cases ---------------------------------------

    #[test]
    fn a_root_spelled_with_a_trailing_separator_is_the_same_root() {
        for spelling in [
            r"C:\coding\agentolis\",
            "C:/coding/agentolis/",
            r"C:\coding\agentolis\.",
            r"C:\coding\other\..\agentolis",
        ] {
            let m = PathMapper::new(Path::new(spelling)).unwrap();
            let (_, p) = m
                .to_logical_str(r"C:\coding\agentolis\src\a.rs")
                .unwrap_or_else(|| panic!("{spelling}"));
            assert_eq!(p.as_str(), "src/a.rs", "{spelling}");
            assert_eq!(
                m.worktree_root(WorktreeId::PRIMARY),
                Some("C:/coding/agentolis")
            );
        }
    }

    #[test]
    fn a_drive_root_and_a_unix_root_are_representable() {
        let m = PathMapper::new(Path::new(r"C:\")).unwrap();
        assert_eq!(m.worktree_root(WorktreeId::PRIMARY), Some("C:"));
        let (_, p) = m.to_logical_str(r"C:\coding\a.rs").unwrap();
        assert_eq!(p.as_str(), "coding/a.rs");
        // A different drive is a different root.
        assert!(m.to_logical_str(r"D:\coding\a.rs").is_none());

        let m = PathMapper::new(Path::new("/")).unwrap();
        assert_eq!(m.worktree_root(WorktreeId::PRIMARY), Some("/"));
        assert_eq!(
            m.to_logical_str("/home/a.rs").unwrap().1.as_str(),
            "home/a.rs"
        );
    }

    #[test]
    fn an_absolute_path_that_climbs_out_of_the_filesystem_root_is_none() {
        let m = mapper();
        for bad in [
            r"C:\coding\..\..\x",
            "/..",
            r"\\srv\share\..\..\x",
            r"\\?\C:\..\..\x",
        ] {
            assert!(m.to_logical_str(bad).is_none(), "{bad}");
        }
        // …and it stays an error rather than a clamp when parsed directly.
        assert_eq!(
            AbsPath::parse(r"C:\a\..\..\b").unwrap_err(),
            PathParseError::EscapesRoot
        );
    }

    #[test]
    fn resolve_normalises_the_cwd_side_too() {
        let m = mapper();
        let cwd = Path::new(r"C:\coding\agentolis\src\");
        assert_eq!(m.resolve(Some(cwd), ".").unwrap().1.as_str(), "src");
        assert_eq!(
            m.resolve(Some(cwd), r".\auth\a.rs").unwrap().1.as_str(),
            "src/auth/a.rs"
        );
        assert_eq!(
            m.resolve(Some(cwd), "日本語/a.rs").unwrap().1.as_str(),
            "src/日本語/a.rs"
        );
        // A cwd outside every root cannot rescue a relative path.
        assert!(m
            .resolve(Some(Path::new(r"D:\elsewhere")), "a.rs")
            .is_none());
    }

    #[test]
    fn a_non_utf8_root_is_an_error_not_a_panic() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let p = Path::new(OsStr::from_bytes(b"/home/\xff"));
            assert_eq!(PathMapper::new(p).unwrap_err(), PathParseError::NotUtf8);
        }
        #[cfg(windows)]
        {
            use std::ffi::OsString;
            use std::os::windows::ffi::OsStringExt;
            let os = OsString::from_wide(&[0x0043, 0x003a, 0x005c, 0xD800]);
            assert_eq!(
                PathMapper::new(Path::new(&os)).unwrap_err(),
                PathParseError::NotUtf8
            );
        }
    }
}
