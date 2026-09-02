//! Growth order from git history (PRD §7.1), worktrees (§7.6), diff counts (§7.3).
//!
//! > **`git log` is the growth order.** Replay it in commit order. Files added in
//! > the repo's first year form the old town — dense, tangled, irregular. Files
//! > added last month sit on the periphery and look more planned. This is not
//! > decoration: the age structure of the codebase becomes visible at a glance.
//!
//! Bootstrap command, verbatim from the PRD:
//!
//! ```text
//! git log --diff-filter=A --name-only --reverse --format=%H|%ct
//! ```
//!
//! Invoked as a subprocess rather than through libgit2: the output is one stable
//! plumbing format, and it avoids a C toolchain on every platform for a job that
//! runs once per `HEAD`.
//!
//! # Three subprocess details that bite
//!
//! * **Paths are git's, not the OS's.** `git log --name-only` emits
//!   forward-slash, repo-relative paths and quotes anything non-ASCII in C-style
//!   escapes unless `core.quotePath` is off. Run with `-c core.quotePath=false`
//!   and parse bytes; a `str` index derived from another string panics on a
//!   multibyte boundary (ADR-0046). `core.quotePath=false` is **not** the whole
//!   story: git still quotes a path containing `"`, `\`, or a control character,
//!   on every platform, so [`unquote_c_style`] is a required fallback and not a
//!   belt-and-braces nicety.
//! * **The output is unbounded.** A large repository's `git log` is tens of
//!   megabytes. Stream it; `Command::output()` buffers the lot. Streaming stdout
//!   while leaving stderr on a pipe deadlocks the moment git fills 64 KiB of
//!   warnings (`LF will be replaced by CRLF`, once per file), so stderr is
//!   drained by a thread.
//! * **A path over `MAX_PATH` needs a `\\?\` prefix before it reaches an external
//!   process**, even though `std::fs` copes without one (ADR-0034).
//!
//! # Measured: git cannot reach a repository past `MAX_PATH` at all
//!
//! The third point above is the PRD's expectation. It does not survive contact:
//! on git 2.53.0.windows.1 with `LongPathsEnabled = 0`, a repository root of 359
//! characters is unreachable by **every** invocation route —
//!
//! | route | result |
//! |---|---|
//! | `git -C <plain>` | `fatal: cannot change to '…': Filename too long` |
//! | `git -C \\?\<plain>` | identical failure — git does not consume the prefix |
//! | `git -c core.longpaths=true -C …` | identical failure |
//! | `git --git-dir=… --work-tree=…` | `fatal: '$GIT_DIR' too big` |
//!
//! `chdir()` and `$GIT_DIR` are length-checked before any config is read, so no
//! flag can rescue it. This module therefore does **not** add a `\\?\` prefix; it
//! *strips* one (a caller holding the output of `fs::canonicalize` has one, and
//! git echoes it back verbatim in the error), and when a long-path failure is
//! recognised it replaces git's message with the actionable one: enable
//! `LongPathsEnabled`, or move the checkout. See [`self_check`].
//!
//! # Determinism (PRD §7.4) constrains the command line, not just the code
//!
//! The growth sequence is the layout's most load-bearing input, so every git
//! setting that could change it is pinned with `-c` on every invocation rather
//! than inherited from the user:
//!
//! * **`diff.renames=false`, plus an explicit `--no-renames`.** Rename detection
//!   is a *heuristic* whose result depends on `diff.renames`, `diff.renameLimit`
//!   and the size of the change — user configuration, and a similarity score.
//!   Two developers would get two different cities from one repository. With
//!   renames off, a rename is a delete plus an add, and the new path enters the
//!   growth sequence at the rename commit: it is a new address, which is exactly
//!   how the layout keys it (PRD §7.6).
//! * **`core.quotePath=false`** so paths arrive as raw UTF-8 bytes.
//! * **`log.showSignature=false`** so a signed-commit configuration cannot
//!   interleave `gpg` output with the parse.
//! * **`--date-order`** rather than git's default traversal: it adds the
//!   guarantee that no parent is shown before its children, which under
//!   `--reverse` means a commit never precedes its own ancestors.
//!
//! # `GIT_OPTIONAL_LOCKS=0` on every call
//!
//! Polis observes a repository that agents are actively committing to. `git diff`
//! would otherwise refresh and rewrite the index, taking `index.lock` — and
//! losing that race makes *the agent's* git command fail, not ours. Read-only
//! observation must never be able to break the thing it observes.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{anyhow, bail, Context};
use polis_events::{LogicalPath, PathMapper, WallTime, WorktreeId};
use serde::{Deserialize, Serialize};

use crate::RepoTree;

// ---------------------------------------------------------------------------
// Invocation.
// ---------------------------------------------------------------------------

/// `-c` settings pinned on every `git` invocation, so that the city does not
/// depend on the operator's `~/.gitconfig` (PRD §7.4).
const CONFIG_PINS: [&str; 5] = [
    "core.quotePath=false",
    "core.longpaths=true",
    "diff.renames=false",
    "log.showSignature=false",
    "core.pager=cat",
];

/// Environment variables that would redirect git at a *different* repository
/// than the one we asked for. Polis is frequently launched from inside a
/// checkout by a tool that sets these.
const ENV_STRIP: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// How much of git's stderr is kept for an error message. The rest is drained
/// and discarded — draining is mandatory (deadlock), keeping it is not.
const STDERR_CAP: usize = 8 * 1024;

/// How far into an untracked file [`diff_counts`] will read before it stops
/// counting lines. Untracked files are not in the index, so git will not count
/// them for us and the count is ours to do; a 2 GiB log file dropped into a
/// working tree must not stall a frame.
pub const UNTRACKED_SCAN_LIMIT: u64 = 8 * 1024 * 1024;

/// How far [`diff_counts`] looks for a `NUL` before calling an untracked file
/// text. Git's own heuristic, and the same number git uses.
const BINARY_PROBE_BYTES: usize = 8000;

/// The `head` value standing for a repository whose `HEAD` has no commit yet.
///
/// A freshly `git init`-ed repository is a normal state, not an error: it has an
/// empty growth sequence and every working-tree file is untracked. The empty
/// string is used rather than `Option<String>` because it flows straight into
/// [`RepoTree::head`] and compares correctly as a cache key.
pub const UNBORN_HEAD: &str = "";

/// Outcome of one `git` subprocess.
#[derive(Debug)]
struct GitRun {
    status: ExitStatus,
    stderr: String,
}

impl GitRun {
    /// Git's stderr, trimmed, for an error message.
    fn message(&self) -> &str {
        self.stderr.trim()
    }

    /// True when git failed for a reason that is really "this path is longer
    /// than Windows lets git address".
    fn is_long_path_failure(&self) -> bool {
        let s = &self.stderr;
        s.contains("Filename too long") || s.contains("$GIT_DIR' too big")
    }

    /// Turns a non-zero exit into an `anyhow` error, replacing git's message
    /// with the actionable one when the cause is a `MAX_PATH` overflow.
    fn check(&self, what: &str, repo_root: &Path) -> anyhow::Result<()> {
        if self.status.success() {
            return Ok(());
        }
        if self.is_long_path_failure() {
            bail!(
                "git cannot open `{}`: the path is longer than Windows' MAX_PATH and git \
                 rejects it through every route (`-C`, `--git-dir`, and a `\\\\?\\` prefix \
                 alike). Enable the `LongPathsEnabled` registry value and reinstall Git for \
                 Windows with long-path support, or move the checkout closer to the drive root.",
                repo_root.display()
            );
        }
        let msg = self.message();
        if msg.is_empty() {
            bail!(
                "`git {what}` failed with {} in {}",
                self.status,
                repo_root.display()
            );
        }
        bail!("`git {what}` failed in {}: {msg}", repo_root.display())
    }
}

/// Renders a repository path as an argument an external `git` will accept.
///
/// Strips a `\\?\` / `\\?\UNC\` verbatim prefix: `std::fs::canonicalize` returns
/// one on Windows, `PathMapper` normalises them away (ADR-0034), and git does
/// not understand them — it treats the prefix as part of the directory name and
/// fails with the prefix visible in its own error message.
fn git_arg_path(path: &Path) -> anyhow::Result<String> {
    let s = path
        .to_str()
        .ok_or_else(|| anyhow!("repository path is not valid UTF-8: {}", path.display()))?;
    Ok(strip_verbatim(s).into_owned())
}

/// `\\?\C:\x` → `C:\x`, `\\?\UNC\server\share` → `\\server\share`, anything
/// else unchanged.
fn strip_verbatim(s: &str) -> Cow<'_, str> {
    // Byte comparison to find the prefix, never `&s[..n]` on an unverified
    // string: a verbatim prefix and a non-ASCII directory name co-occur
    // constantly on Windows and `str` indexing panics off a character boundary
    // (ADR-0046). Every index used below lands immediately after an ASCII byte
    // this function has just verified, which is what makes the slicing safe.
    let b = s.as_bytes();
    if !b.starts_with(br"\\?\") {
        return Cow::Borrowed(s);
    }
    let rest = &b[4..];
    if rest.len() >= 4 && rest[..3].eq_ignore_ascii_case(b"UNC") && rest[3] == b'\\' {
        // Re-form `\\server\share`: the two leading separators are what makes it
        // a UNC path again, so this one case has to allocate.
        return Cow::Owned(format!(r"\\{}", &s[8..]));
    }
    Cow::Borrowed(&s[4..])
}

/// Runs `git` in `repo_root`, handing each `delim`-terminated record to
/// `on_record` as it arrives.
///
/// Streaming, not buffering: `git log` over a large history is tens of megabytes
/// and `Command::output()` would hold all of it. stderr is drained on a thread
/// because git emits one `LF will be replaced by CRLF` line per file on a
/// `core.autocrlf` checkout, which fills the 64 KiB pipe buffer and deadlocks a
/// reader that only pulls on stdout.
fn stream_git<F>(
    repo_root: &Path,
    args: &[&str],
    delim: u8,
    mut on_record: F,
) -> anyhow::Result<GitRun>
where
    F: FnMut(&[u8]) -> anyhow::Result<()>,
{
    let root = git_arg_path(repo_root)?;

    let mut cmd = Command::new("git");
    cmd.arg("--no-pager");
    for pin in CONFIG_PINS {
        cmd.arg("-c").arg(pin);
    }
    cmd.arg("-C").arg(&root);
    cmd.args(args);
    for key in ENV_STRIP {
        cmd.env_remove(key);
    }
    cmd.env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "failed to spawn `git` (is it on PATH?) for {}",
            repo_root.display()
        )
    })?;

    let mut err_pipe = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("git stderr was not piped"))?;
    let err_thread = std::thread::spawn(move || {
        let mut kept: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match err_pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let remaining = STDERR_CAP.saturating_sub(kept.len());
                    if remaining > 0 {
                        kept.extend_from_slice(&chunk[..n.min(remaining)]);
                    }
                }
            }
        }
        String::from_utf8_lossy(&kept).into_owned()
    });

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("git stdout was not piped"))?;
    let mut reader = BufReader::with_capacity(64 * 1024, stdout);
    let mut record = Vec::new();
    let mut outcome: anyhow::Result<()> = Ok(());
    loop {
        record.clear();
        match reader.read_until(delim, &mut record) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                outcome = Err(anyhow::Error::from(e).context("reading git output"));
                break;
            }
        }
        if record.last() == Some(&delim) {
            record.pop();
        }
        if delim == b'\n' && record.last() == Some(&b'\r') {
            record.pop();
        }
        if let Err(e) = on_record(&record) {
            outcome = Err(e);
            break;
        }
    }

    // Drain whatever is left so the child can exit instead of blocking on a full
    // stdout pipe; without this an early `break` leaks a live `git`.
    if outcome.is_err() {
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    }
    drop(reader);

    let status = child.wait().context("waiting for git")?;
    let stderr = err_thread.join().unwrap_or_default();
    outcome?;
    Ok(GitRun { status, stderr })
}

/// Runs `git` and returns its stdout as a `String`, for the short outputs
/// (`rev-parse`, `worktree list`) where streaming buys nothing.
fn capture_git(repo_root: &Path, args: &[&str]) -> anyhow::Result<(GitRun, String)> {
    let mut out = Vec::new();
    let run = stream_git(repo_root, args, b'\n', |line| {
        out.extend_from_slice(line);
        out.push(b'\n');
        Ok(())
    })?;
    Ok((run, String::from_utf8_lossy(&out).into_owned()))
}

/// Checks that `git` is present and can actually open this repository.
///
/// ADR-0034's "startup self-checks by actually reading" applied to git: the
/// long-path failure this catches has no workaround, so it must be a startup
/// answer rather than a runtime surprise three seconds into the first frame.
pub fn self_check(repo_root: &Path) -> anyhow::Result<()> {
    let (run, _) = capture_git(repo_root, &["rev-parse", "--git-dir"])?;
    run.check("rev-parse --git-dir", repo_root)
}

/// True when the checkout is a shallow clone.
///
/// Shallow clones need no special handling in the growth walk — the grafted
/// boundary commit is parentless, so `--diff-filter=A` reports its **entire
/// tree** as added, which is precisely the "old town" reading Polis wants. This
/// is exposed so the UI can say the history is truncated rather than let the
/// operator read a compressed old town as fact.
pub fn is_shallow(repo_root: &Path) -> anyhow::Result<bool> {
    let (run, out) = capture_git(repo_root, &["rev-parse", "--is-shallow-repository"])?;
    run.check("rev-parse --is-shallow-repository", repo_root)?;
    Ok(out.trim() == "true")
}

// ---------------------------------------------------------------------------
// Path decoding.
// ---------------------------------------------------------------------------

/// Decodes one path as git wrote it into a [`LogicalPath`].
///
/// `None` for a path that cannot become a layout key: empty, non-UTF-8
/// (ADR-0028 rejects rather than lossily converting — `to_string_lossy` maps two
/// different files onto one key through `U+FFFD`), or malformed quoting.
fn decode_path(raw: &[u8]) -> Option<LogicalPath> {
    if raw.is_empty() {
        return None;
    }
    let bytes: Cow<'_, [u8]> = if raw.first() == Some(&b'"') {
        Cow::Owned(unquote_c_style(raw)?)
    } else {
        Cow::Borrowed(raw)
    };
    let text = std::str::from_utf8(&bytes).ok()?;
    LogicalPath::new(text).ok()
}

/// Same as [`decode_path`], logging what it dropped.
fn decode_path_logged(raw: &[u8]) -> Option<LogicalPath> {
    let decoded = decode_path(raw);
    if decoded.is_none() && !raw.is_empty() {
        tracing::debug!(
            path = %String::from_utf8_lossy(raw),
            "git reported a path that cannot be a layout key; it gets no building"
        );
    }
    decoded
}

/// Reverses git's `quote_c_style`.
///
/// `core.quotePath=false` stops git quoting bytes ≥ `0x80`, and nothing else:
/// a path containing `"`, `\`, or a control character is still wrapped in double
/// quotes with C escapes, on every platform and at every setting. Returns `None`
/// when the input is not a well-formed quoted string.
///
/// Operates on bytes throughout. The escaped form is `\nnn` **octal** — not hex,
/// and not `\u` — so a UTF-8 path arrives as a run of octal escapes that only
/// reassembles into a character after decoding (ADR-0046).
pub fn unquote_c_style(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.len() < 2 || raw.first() != Some(&b'"') || raw.last() != Some(&b'"') {
        return None;
    }
    let body = &raw[1..raw.len() - 1];
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        let b = body[i];
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        i += 1;
        let esc = *body.get(i)?;
        i += 1;
        match esc {
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0C),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0B),
            b'"' | b'\\' => out.push(esc),
            b'0'..=b'7' => {
                // Git writes exactly three octal digits; accept one to three so
                // a hand-written fixture is not rejected on a technicality.
                let mut value = u32::from(esc - b'0');
                let mut digits = 1;
                while digits < 3 {
                    match body.get(i) {
                        Some(&d @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(d - b'0');
                            i += 1;
                            digits += 1;
                        }
                        _ => break,
                    }
                }
                out.push(u8::try_from(value).ok()?);
            }
            // An unknown escape is not something git emits; taking the character
            // literally loses less than refusing the whole path would.
            other => out.push(other),
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// The growth sequence.
// ---------------------------------------------------------------------------

/// The order in which files first appeared, oldest first.
///
/// Cached keyed on `HEAD` and recomputed incrementally on new commits. This is
/// PRD §13.1's cold-start budget: a full walk of a large history is seconds, and
/// the cache is what keeps the second launch under 3 s.
///
/// # Entries are unique by logical path
///
/// A path appears **once**, at its *first* addition — PRD §7.1 defines the
/// growth index as "the index of the commit that first added this file". A file
/// deleted and re-added keeps its original position, which is the right reading:
/// the address is old even if the current occupant is not.
///
/// Uniqueness is what makes [`extend_to_head`](Self::extend_to_head) trivially
/// correct — appending a path already present is a no-op, so a commit range
/// applied twice cannot shift a single building.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GrowthSequence {
    /// `(path, commit time)` in commit order. The index into this vector is
    /// [`crate::FileMeta::growth_index`].
    pub entries: Vec<(LogicalPath, WallTime)>,
    /// The `HEAD` this was derived from. A mismatch invalidates the cache.
    pub head: String,
}

impl GrowthSequence {
    /// Runs the bootstrap command and parses its output.
    ///
    /// `HEAD` is read *first* and the walk is pinned to that commit, so a commit
    /// landing mid-walk cannot produce a sequence that covers more history than
    /// its own cache key claims.
    pub fn bootstrap(repo_root: &Path) -> anyhow::Result<Self> {
        let head = head_commit(repo_root)?;
        if head.is_empty() {
            return Ok(Self {
                entries: Vec::new(),
                head,
            });
        }
        let mut seq = Self {
            entries: Vec::new(),
            head,
        };
        let rev = seq.head.clone();
        let mut seen = BTreeSet::new();
        seq.walk_adds(repo_root, &rev, &mut seen)?;
        Ok(seq)
    }

    /// Extends a cached sequence with commits made since [`GrowthSequence::head`].
    ///
    /// Returns how many entries were appended. Never reorders existing entries:
    /// a growth index that moved would move every building placed after it.
    ///
    /// # When the promise cannot be kept
    ///
    /// A rebase, an amend, or a reset moves `HEAD` somewhere that is not a
    /// descendant of the cached one, and the history the city was built from no
    /// longer exists. The sequence is then **rebuilt** from scratch, and indices
    /// may move. That is deliberate: the alternative — grafting the surviving
    /// paths onto the old order — would make an incrementally-updated Polis and a
    /// freshly-launched Polis disagree about the same repository, which is
    /// exactly what PRD §7.4 forbids. A rewritten history is the one case where
    /// the city legitimately changes.
    pub fn extend_to_head(&mut self, repo_root: &Path) -> anyhow::Result<usize> {
        let head = head_commit(repo_root)?;
        if head == self.head {
            return Ok(0);
        }
        if head.is_empty() {
            // The repository was rewound to an unborn HEAD. Nothing is added.
            self.entries.clear();
            self.head = head;
            return Ok(0);
        }
        if self.head.is_empty() || !is_ancestor(repo_root, &self.head, &head)? {
            let before = self.entries.len();
            let rebuilt = Self::bootstrap(repo_root)?;
            *self = rebuilt;
            return Ok(self.entries.len().saturating_sub(before));
        }

        let range = format!("{}..{head}", self.head);
        let before = self.entries.len();
        let mut seen: BTreeSet<LogicalPath> = self.entries.iter().map(|(p, _)| p.clone()).collect();
        self.walk_adds(repo_root, &range, &mut seen)?;
        self.head = head;
        Ok(self.entries.len() - before)
    }

    /// The shared `git log` walk: appends every first-addition in `rev` that is
    /// not already in `seen`, in commit order.
    fn walk_adds(
        &mut self,
        repo_root: &Path,
        rev: &str,
        seen: &mut BTreeSet<LogicalPath>,
    ) -> anyhow::Result<()> {
        let entries = &mut self.entries;
        let run = walk_log(repo_root, rev, &["--diff-filter=A"], |time, raw| {
            if let Some(path) = decode_path_logged(raw) {
                if seen.insert(path.clone()) {
                    entries.push((path, time));
                }
            }
        })?;
        run.check("log --diff-filter=A --name-only", repo_root)
    }

    /// Loads a cached sequence, if one exists and its `HEAD` still matches.
    ///
    /// Every failure — absent, unreadable, truncated, written by an older
    /// format, or keyed on a different `HEAD` — is a **miss**, not an error. A
    /// cache that can fail a launch is worse than no cache.
    pub fn load_cached(cache: &Path, head: &str) -> Option<Self> {
        let bytes = std::fs::read(cache).ok()?;
        let seq: Self = serde_json::from_slice(&bytes).ok()?;
        if seq.head == head {
            Some(seq)
        } else {
            None
        }
    }

    /// Writes the cache atomically (temp + rename).
    ///
    /// The serialisation is byte-stable: `entries` is a `Vec` whose order is the
    /// growth order itself, `head` is a fixed-width hex string, and serde emits
    /// struct fields in declaration order. Writing the same sequence twice
    /// produces identical bytes, which is what makes PRD §16's golden-file
    /// comparison across two operating systems meaningful.
    pub fn save_cached(&self, cache: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        write_atomic(cache, &bytes)
    }

    /// The growth index of a path, if git has ever seen it.
    ///
    /// Linear. Use [`index_map`](Self::index_map) to fill a whole tree — a
    /// per-file call over a 50 000-file repository is quadratic.
    pub fn index_of(&self, path: &LogicalPath) -> Option<u32> {
        let i = self.entries.iter().position(|(p, _)| p == path)?;
        u32::try_from(i).ok()
    }

    /// Commit time of a path's first addition.
    pub fn added_at(&self, path: &LogicalPath) -> Option<WallTime> {
        self.entries
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, t)| *t)
    }

    /// Path → growth index, for filling a whole [`RepoTree`] in one pass.
    ///
    /// `BTreeMap`, not `HashMap`: this map is iterated where a layout can see it,
    /// and `ahash`'s `RandomState` is seeded per process (PRD §7.4).
    pub fn index_map(&self) -> BTreeMap<LogicalPath, u32> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, (p, _))| Some((p.clone(), u32::try_from(i).ok()?)))
            .collect()
    }

    /// Number of files git has ever added.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True for a repository with no commits, or none that added a file.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// True when this sequence came from a repository whose `HEAD` has no commit.
    pub fn is_unborn(&self) -> bool {
        self.head == UNBORN_HEAD
    }
}

/// Streams `git log … <rev>` and calls `on_path` for every file in every commit,
/// oldest commit first, paired with that commit's committer time.
///
/// The header/file grammar is
///
/// ```text
/// <sha>|<committer time>
/// <blank>
/// <path>
/// <path>
/// <sha>|<committer time>
/// …
/// ```
///
/// with **no** blank line before the next header. Distinguishing a header from a
/// path is therefore a parse decision, not a delimiter: a header is 40 or 64 hex
/// digits, a `|`, and an integer, and the first non-blank line after a header can
/// only ever be a path. Both guards are applied.
fn walk_log(
    repo_root: &Path,
    rev: &str,
    extra: &[&str],
    on_path: impl FnMut(WallTime, &[u8]),
) -> anyhow::Result<GitRun> {
    walk_log_with(repo_root, rev, "--name-only", extra, on_path)
}

/// [`walk_log`] over `--name-status`, so one pass carries both PRD §7.1's
/// first-additions and PRD §8's last-touched times.
///
/// The line grammar gains one field: a file line is `<status>\t<path>`, where
/// `<status>` is git's diff-filter letter — `A`, `M`, `D`, `T`. With
/// `--no-renames` pinned (see the module docs) there is no `R<score>` form and
/// therefore never a second tab, so the split is unambiguous.
///
/// **`status == b'A'` is exactly `--diff-filter=A`**: the filter selects on the
/// same letter this prints. `the_fused_walk_agrees_with_the_two_pinned_commands`
/// asserts that on real repositories rather than trusting it.
fn walk_log_status(
    repo_root: &Path,
    rev: &str,
    mut on_entry: impl FnMut(WallTime, u8, &[u8]),
) -> anyhow::Result<GitRun> {
    walk_log_with(repo_root, rev, "--name-status", &[], |time, line| {
        if let Some((status, path)) = split_status(line) {
            on_entry(time, status, path);
        }
    })
}

/// Splits a `--name-status` file line into its status letter and its path.
///
/// `None` for a line with no tab or an empty path — neither is a file, and a
/// malformed line must be dropped rather than parsed as a path called `A`.
fn split_status(line: &[u8]) -> Option<(u8, &[u8])> {
    let tab = line.iter().position(|&b| b == b'\t')?;
    let status = *line.first()?;
    let path = line.get(tab + 1..)?;
    if path.is_empty() {
        return None;
    }
    Some((status, path))
}

/// The shared walk. `name_flag` selects the diff output format; it is one flag
/// and not part of `extra` because passing both `--name-only` and
/// `--name-status` is a silent last-one-wins in git.
fn walk_log_with(
    repo_root: &Path,
    rev: &str,
    name_flag: &str,
    extra: &[&str],
    mut on_path: impl FnMut(WallTime, &[u8]),
) -> anyhow::Result<GitRun> {
    let mut args: Vec<&str> = vec![
        "log",
        name_flag,
        "--no-renames",
        "--no-diff-merges",
        "--reverse",
        "--date-order",
        "--format=%H|%ct",
    ];
    args.extend_from_slice(extra);
    args.push(rev);
    args.push("--");

    let mut time = WallTime::UNIX_EPOCH;
    let mut expect_path = false;
    stream_git(repo_root, &args, b'\n', |line| {
        match classify_log_line(line, &mut expect_path) {
            LogLine::Blank => {}
            LogLine::Header(t) => time = t,
            LogLine::Path(p) => on_path(time, p),
        }
        Ok(())
    })
}

/// One classified line of `walk_log`'s output.
#[derive(Debug, PartialEq, Eq)]
enum LogLine<'a> {
    /// The separator git puts between a commit header and its file list.
    Blank,
    /// A commit header, carrying its committer time.
    Header(WallTime),
    /// A file name, exactly as git wrote it.
    Path(&'a [u8]),
}

/// Decides whether a line is a header or a path.
///
/// `expect_path` carries the one piece of state that makes this unambiguous: the
/// first non-blank line after a header is always a file name, whatever it looks
/// like. Without it a file literally named `<40 hex digits>|<digits>` — legal on
/// every filesystem that allows `|` — would be silently eaten as a header and
/// every file after it in that commit misdated.
fn classify_log_line<'a>(line: &'a [u8], expect_path: &mut bool) -> LogLine<'a> {
    if line.is_empty() {
        return LogLine::Blank;
    }
    if !*expect_path {
        if let Some(t) = parse_header(line) {
            *expect_path = true;
            return LogLine::Header(t);
        }
    }
    *expect_path = false;
    LogLine::Path(line)
}

/// Parses a `%H|%ct` header line, or `None` when the line is a path.
fn parse_header(line: &[u8]) -> Option<WallTime> {
    let bar = line.iter().position(|&b| b == b'|')?;
    let (hash, rest) = line.split_at(bar);
    // SHA-1 is 40 hex digits, SHA-256 is 64. Anything else is a filename that
    // happens to contain a bar.
    if hash.len() != 40 && hash.len() != 64 {
        return None;
    }
    if !hash.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let secs = std::str::from_utf8(&rest[1..]).ok()?;
    // Signed: a rewritten history can carry a pre-1970 commit, and `WallTime` is
    // signed precisely so it stays representable (ADR-0048).
    Some(WallTime::from_unix_seconds(secs.parse::<i64>().ok()?))
}

/// True when `ancestor` is reachable from `descendant`.
///
/// A missing commit — garbage-collected after a rebase — reports `false` rather
/// than failing: the caller's next move is a rebuild either way.
fn is_ancestor(repo_root: &Path, ancestor: &str, descendant: &str) -> anyhow::Result<bool> {
    let (run, _) = capture_git(
        repo_root,
        &["merge-base", "--is-ancestor", ancestor, descendant],
    )?;
    Ok(run.status.success())
}

// ---------------------------------------------------------------------------
// HEAD.
// ---------------------------------------------------------------------------

/// Reads `HEAD` for a checkout.
///
/// Returns the commit id, not a branch name: a detached `HEAD` is not a branch,
/// and treating the literal string `HEAD` as one makes two unrelated checkouts
/// look like the same branch for contention tiering (ADR-0018).
///
/// A repository with no commits yet returns [`UNBORN_HEAD`] — the empty string —
/// rather than an error. That is a normal state for a repository an agent has
/// just created, and it flows straight into [`RepoTree::head`] as a cache key
/// that correctly invalidates on the first commit.
pub fn head_commit(repo_root: &Path) -> anyhow::Result<String> {
    Ok(head_commit_opt(repo_root)?.unwrap_or_default())
}

/// [`head_commit`] with the unborn case spelled out.
pub fn head_commit_opt(repo_root: &Path) -> anyhow::Result<Option<String>> {
    let (run, out) = capture_git(repo_root, &["rev-parse", "--verify", "--quiet", "HEAD"])?;
    // `--verify --quiet` exits 1 with no output for an unborn HEAD, and 128 for
    // a real failure (not a repository, git unusable).
    if run.status.code() == Some(1) && run.message().is_empty() {
        return Ok(None);
    }
    run.check("rev-parse --verify HEAD", repo_root)?;
    let head = out.trim().to_owned();
    if head.is_empty() {
        return Ok(None);
    }
    Ok(Some(head))
}

/// The last commit time per file, for PRD §8's overgrowth.
///
/// One `git log --name-only --format=%ct` pass in **forward** order, so the
/// newest commit touching a path is the last write and wins. Folded into
/// [`crate::FileMeta::last_touched`].
///
/// Includes deletions: a deleted file's last-touched is the commit that removed
/// it, which is what makes PRD §7.5's vacant lots age.
pub fn last_touched(repo_root: &Path) -> anyhow::Result<Vec<(LogicalPath, WallTime)>> {
    Ok(last_touched_map(repo_root)?.into_iter().collect())
}

/// [`last_touched`] as the map it is built from, sorted by logical path.
pub fn last_touched_map(repo_root: &Path) -> anyhow::Result<BTreeMap<LogicalPath, WallTime>> {
    let head = head_commit(repo_root)?;
    let mut map = BTreeMap::new();
    if head.is_empty() {
        return Ok(map);
    }
    last_touched_into(repo_root, &head, &mut map)?;
    Ok(map)
}

/// Accumulates last-touched times for one revision range into `map`.
///
/// Forward order means a later commit simply overwrites an earlier one, so
/// applying a range on top of an existing map is correct without a comparison.
fn last_touched_into(
    repo_root: &Path,
    rev: &str,
    map: &mut BTreeMap<LogicalPath, WallTime>,
) -> anyhow::Result<()> {
    let run = walk_log(repo_root, rev, &[], |time, raw| {
        if let Some(path) = decode_path_logged(raw) {
            map.insert(path, time);
        }
    })?;
    run.check("log --name-only", repo_root)
}

// ---------------------------------------------------------------------------
// Worktrees (PRD §7.6).
// ---------------------------------------------------------------------------

/// Enumerates `git worktree list --porcelain` (PRD §7.6).
///
/// This, `SessionStart`'s `cwd`, and `CwdChanged` are the three ways Polis
/// learns about a worktree. Never the `WorktreeCreate` hook, which Polis must not
/// register — it would break every worktree on the machine (ADR-0002).
///
/// The **main** worktree is always [`WorktreeId::PRIMARY`], whichever checkout
/// `repo_root` points at: git lists it first, and the city is keyed to it.
/// Linked worktrees are returned sorted by path and take ids derived from a
/// written-out hash of that path (see [`worktree_id_for`]), so adding or
/// removing one never renumbers the others.
pub fn list_worktrees(repo_root: &Path) -> anyhow::Result<Vec<Worktree>> {
    let (run, out) = capture_git(repo_root, &["worktree", "list", "--porcelain"])?;
    run.check("worktree list --porcelain", repo_root)?;
    Ok(assign_worktree_ids(parse_worktree_porcelain(&out)))
}

/// One record of `git worktree list --porcelain`, before ids are assigned.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawWorktree {
    path: String,
    branch: Option<String>,
    head: String,
}

/// Parses the porcelain records. Unknown keys (`locked`, `prunable`, and
/// whatever a later git adds) are ignored rather than refused.
fn parse_worktree_porcelain(text: &str) -> Vec<RawWorktree> {
    let mut out = Vec::new();
    let mut current: Option<RawWorktree> = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            out.extend(current.take());
            continue;
        }
        let (key, value) = match line.split_once(' ') {
            Some((k, v)) => (k, v),
            None => (line, ""),
        };
        match key {
            "worktree" => {
                out.extend(current.take());
                current = Some(RawWorktree {
                    path: value.to_owned(),
                    branch: None,
                    head: String::new(),
                });
            }
            "HEAD" => {
                if let Some(w) = current.as_mut() {
                    // A worktree on an unborn branch reports all zeroes; that is
                    // the same "no commit yet" state as `UNBORN_HEAD`.
                    w.head = if value.bytes().all(|b| b == b'0') {
                        UNBORN_HEAD.to_owned()
                    } else {
                        value.to_owned()
                    };
                }
            }
            "branch" => {
                if let Some(w) = current.as_mut() {
                    w.branch = Some(
                        value
                            .strip_prefix("refs/heads/")
                            .unwrap_or(value)
                            .to_owned(),
                    );
                }
            }
            // `detached` and `bare` carry no value; `bare` additionally means the
            // record has no HEAD line, which the empty default already covers.
            _ => {}
        }
    }
    out.extend(current.take());
    out
}

/// Assigns [`WorktreeId`]s: the main worktree is `PRIMARY`, the rest are hashed.
fn assign_worktree_ids(raw: Vec<RawWorktree>) -> Vec<Worktree> {
    let mut iter = raw.into_iter();
    let mut out = Vec::new();
    let mut used: BTreeSet<u32> = BTreeSet::new();
    used.insert(WorktreeId::PRIMARY.0);

    if let Some(main) = iter.next() {
        out.push(Worktree {
            id: WorktreeId::PRIMARY,
            path: PathBuf::from(&main.path),
            branch: main.branch,
            head: main.head,
        });
    }

    // Sorted so that collision resolution — and therefore every id — is
    // independent of the order git happened to list them in (PRD §7.4).
    let mut rest: Vec<RawWorktree> = iter.collect();
    rest.sort_by_cached_key(|w| normalize_root(&w.path));
    for w in rest {
        let mut id = worktree_id_for(&w.path).0;
        while !used.insert(id) {
            id = id.wrapping_add(1);
            if id == WorktreeId::PRIMARY.0 {
                id = 1;
            }
        }
        out.push(Worktree {
            id: WorktreeId(id),
            path: PathBuf::from(&w.path),
            branch: w.branch,
            head: w.head,
        });
    }
    out
}

/// The [`WorktreeId`] a linked checkout gets, derived from its path alone.
///
/// Written out rather than delegated to a crate default, for ADR-0029's reason:
/// `DefaultHasher`'s algorithm is unspecified and may change between compiler
/// releases, and a worktree tint that moved when the toolchain moved would be a
/// silent change to what the operator is looking at. FNV-1a over the normalised,
/// ASCII-lowercased path, folded to 32 bits, with zero mapped away from
/// [`WorktreeId::PRIMARY`].
///
/// Path-derived rather than positional so that adding a worktree does not
/// renumber — and re-tint — the ones already on screen.
pub fn worktree_id_for(root: &str) -> WorktreeId {
    let h = fnv1a64(normalize_root(root).as_bytes());
    let folded = u32::try_from((h ^ (h >> 32)) & 0xFFFF_FFFF).unwrap_or(1);
    WorktreeId(if folded == WorktreeId::PRIMARY.0 {
        1
    } else {
        folded
    })
}

/// Forward slashes, no trailing separator, ASCII case folded — the same folding
/// [`LogicalPath`] applies (ADR-0028), so a path is one worktree however the
/// channel that delivered it spelled it.
pub(crate) fn normalize_root(root: &str) -> String {
    let mut s: String = strip_verbatim(root)
        .chars()
        .map(|c| {
            if c == '\\' {
                '/'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    s
}

/// FNV-1a, 64-bit. Written out; see [`worktree_id_for`].
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// One checkout of the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// Stable id used as a rendering dimension, not as part of the layout key.
    pub id: WorktreeId,
    /// Absolute path of this checkout.
    pub path: PathBuf,
    /// Branch name, or `None` when detached.
    ///
    /// 4 463 records in the local corpus carry `gitBranch = "HEAD"`. Two records
    /// both reading `HEAD` are **not** necessarily on the same branch, so
    /// contention severity must not treat detached heads as equal
    /// (PRD §11.3, ADR-0018).
    pub branch: Option<String>,
    /// The commit this checkout is on. The only thing that actually
    /// distinguishes two detached heads. [`UNBORN_HEAD`] for a checkout with no
    /// commit yet, and for a bare main worktree.
    pub head: String,
}

impl Worktree {
    /// True when this checkout is not on a named branch.
    ///
    /// PRD §11.3's "different worktrees/branches, same logical file" tier must
    /// not fire on two detached heads just because both report `HEAD`.
    pub fn is_detached(&self) -> bool {
        self.branch.is_none()
    }

    /// True when two checkouts are on the same branch — never true for two
    /// detached heads on the same commit, and never true for two detached heads
    /// at all (ADR-0018).
    pub fn same_branch_as(&self, other: &Self) -> bool {
        match (&self.branch, &other.branch) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }
}

/// Every checkout of one repository, and the mapping from a physical path to the
/// logical one the city is keyed on (PRD §7.6).
///
/// > `/repo-wt-3/src/auth.ts` and `/repo-wt-7/src/auth.ts` are the **same
/// > logical file in two physical places**. […] Getting this wrong means seven
/// > near-identical maps side by side and the loss of the one thing you most
/// > want to see: two agents editing the same file on different branches.
///
/// The normalisation itself belongs to [`PathMapper`] in `polis-events` — it is
/// the shared contract every channel resolves paths through, and a second
/// implementation here is how two channels come to disagree about which building
/// an edit landed on. This type only supplies it with what `git worktree list`
/// found, and adds the branch, which `PathMapper` deliberately does not carry.
#[derive(Debug, Clone)]
pub struct WorktreeSet {
    worktrees: Vec<Worktree>,
    mapper: PathMapper,
}

impl WorktreeSet {
    /// Enumerates the repository's checkouts and builds the path mapper.
    pub fn discover(repo_root: &Path) -> anyhow::Result<Self> {
        Self::from_worktrees(list_worktrees(repo_root)?)
    }

    /// Builds a set from already-enumerated worktrees.
    ///
    /// The first entry is treated as the main checkout, matching
    /// [`list_worktrees`]. A worktree whose path is not valid UTF-8 or is not
    /// absolute is dropped with a warning rather than failing the whole set: one
    /// unmappable checkout should cost that checkout's tint, not the city.
    pub fn from_worktrees(worktrees: Vec<Worktree>) -> anyhow::Result<Self> {
        let primary = worktrees
            .first()
            .ok_or_else(|| anyhow!("git reported no worktrees at all"))?;
        let mut mapper = PathMapper::new(&primary.path).with_context(|| {
            format!("main worktree path is unusable: {}", primary.path.display())
        })?;
        for w in worktrees.iter().skip(1) {
            if let Err(e) = mapper.add_worktree(w.id, &w.path) {
                tracing::warn!(
                    worktree = %w.path.display(),
                    error = %e,
                    "worktree path could not be normalised; its edits will not map to buildings"
                );
            }
        }
        Ok(Self { worktrees, mapper })
    }

    /// The main checkout. [`RepoTree::root`] and the layout are keyed to this
    /// one; every other checkout is a tint over it.
    pub fn primary(&self) -> &Worktree {
        // `from_worktrees` refuses an empty list, so this cannot be absent.
        &self.worktrees[0]
    }

    /// Every checkout, main first then linked ones in path order.
    pub fn worktrees(&self) -> &[Worktree] {
        &self.worktrees
    }

    /// One checkout by id.
    pub fn get(&self, id: WorktreeId) -> Option<&Worktree> {
        self.worktrees.iter().find(|w| w.id == id)
    }

    /// The branch a checkout is on, or `None` when detached or unknown.
    pub fn branch(&self, id: WorktreeId) -> Option<&str> {
        self.get(id)?.branch.as_deref()
    }

    /// The shared normaliser, for callers that need to resolve paths themselves.
    pub fn mapper(&self) -> &PathMapper {
        &self.mapper
    }

    /// Maps any physical path onto the checkout it belongs to and the logical
    /// path the city is keyed on — the worktree prefix stripped.
    ///
    /// `None` for a path outside every checkout: a transcript under `~/.claude`,
    /// a temp file, `C:\Windows`. That is a normal condition, not an error —
    /// those files have no building.
    pub fn to_logical(&self, path: &Path) -> Option<(WorktreeId, LogicalPath)> {
        self.mapper.to_logical(path)
    }

    /// [`Self::to_logical`] for a path that may be relative, resolved against a
    /// working directory first — which is how tool inputs actually arrive.
    pub fn resolve(&self, cwd: Option<&Path>, path: &str) -> Option<(WorktreeId, LogicalPath)> {
        self.mapper.resolve(cwd, path)
    }

    /// The map [`RepoTree::worktrees`] wants.
    pub fn roots(&self) -> BTreeMap<WorktreeId, PathBuf> {
        self.worktrees
            .iter()
            .map(|w| (w.id, w.path.clone()))
            .collect()
    }

    /// Fills in a tree's checkout dimension: [`RepoTree::root`] and
    /// [`RepoTree::worktrees`]. Adds no files and no geometry — a worktree is a
    /// tint over one shared base map, never a second city.
    pub fn apply(&self, tree: &mut RepoTree) {
        tree.root.clone_from(&self.primary().path);
        tree.worktrees = self.roots();
    }
}

// ---------------------------------------------------------------------------
// Uncommitted diff counts (PRD §7.3).
// ---------------------------------------------------------------------------

/// What one file contributes to PRD §7.3's building height.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DiffCount {
    /// Lines added since `HEAD`.
    pub added: u32,
    /// Lines removed since `HEAD`.
    pub deleted: u32,
    /// True when git reported `-` for both counts — a binary file, which has no
    /// meaningful line count and contributes no height.
    pub binary: bool,
    /// True when the file is not in the index at all, so the counts were made by
    /// reading it rather than by git.
    pub untracked: bool,
}

impl DiffCount {
    /// Total churn — what PRD §7.3's height is proportional to.
    ///
    /// Added *plus* deleted: "the tallest thing on the map is the biggest
    /// unreviewed pile", and a 400-line deletion is as much to review as a
    /// 400-line addition.
    pub fn lines(self) -> u32 {
        self.added.saturating_add(self.deleted)
    }

    /// True when the file has no uncommitted change worth drawing.
    pub fn is_empty(self) -> bool {
        self.lines() == 0 && !self.binary
    }
}

/// Uncommitted diff line counts per file — PRD §7.3's building height.
///
/// The transcript already carries this three ways, so this is the *fallback*,
/// not the primary source: `structuredPatch` gives exact ±counts per edit,
/// `toolUseResult.toolStats` gives a per-subagent roll-up, and `cost-state`
/// gives a per-session total. `structuredPatch` is missing for subagent edits,
/// which is why the fallback exists at all (ADR-0004, ADR-0042).
///
/// Sorted by logical path. See [`diff_counts`] for the ± split and the binary
/// and untracked flags.
pub fn diff_line_counts(repo_root: &Path) -> anyhow::Result<Vec<(LogicalPath, u32)>> {
    Ok(diff_counts(repo_root)?
        .into_iter()
        .map(|(p, c)| (p, c.lines()))
        .collect())
}

/// [`diff_line_counts`] with the detail the drill-down panel wants.
///
/// One `git diff --numstat -z HEAD` covers staged *and* unstaged changes in a
/// single pass against the last commit, which is the "unreviewed pile" the
/// operator is being shown — summing `git diff` and `git diff --cached` would
/// instead double-count a file that is both staged and edited again.
///
/// Untracked files are added separately: git will not count a file it does not
/// know about, and a brand-new 400-line file is exactly the pile PRD §7.3 wants
/// tall. Their line count is read from disk, capped at [`UNTRACKED_SCAN_LIMIT`].
pub fn diff_counts(repo_root: &Path) -> anyhow::Result<BTreeMap<LogicalPath, DiffCount>> {
    let mut out: BTreeMap<LogicalPath, DiffCount> = BTreeMap::new();
    let head = head_commit(repo_root)?;

    // Against HEAD when there is one; against the index alone when the
    // repository has no commit yet, where `git diff HEAD` is a fatal error.
    let args: Vec<&str> = if head.is_empty() {
        vec!["diff", "--numstat", "-z", "--no-renames", "--cached"]
    } else {
        vec!["diff", "--numstat", "-z", "--no-renames", head.as_str()]
    };

    let mut pending_rename = 0u8;
    let run = stream_git(repo_root, &args, 0, |record| {
        if record.is_empty() {
            return Ok(());
        }
        // A rename record is `added\tdeleted\0old\0new\0`. `--no-renames` makes
        // it unreachable; handling it anyway costs four lines and turns a future
        // flag change from silent corruption into a skipped entry.
        if pending_rename > 0 {
            pending_rename -= 1;
            return Ok(());
        }
        let Some((added, deleted, path_bytes)) = parse_numstat_record(record) else {
            return Ok(());
        };
        if path_bytes.is_empty() {
            pending_rename = 2;
            return Ok(());
        }
        if let Some(path) = decode_path_logged(path_bytes) {
            out.insert(
                path,
                DiffCount {
                    added: added.unwrap_or(0),
                    deleted: deleted.unwrap_or(0),
                    binary: added.is_none() && deleted.is_none(),
                    untracked: false,
                },
            );
        }
        Ok(())
    })?;
    run.check("diff --numstat", repo_root)?;

    for path in untracked_files(repo_root)? {
        let count = count_untracked_lines(&repo_root.join(path.as_str()));
        out.insert(path, count);
    }
    Ok(out)
}

/// Splits one `--numstat -z` record into `(added, deleted, path)`.
///
/// `None` in a count position is git's `-`, which means the file is binary.
fn parse_numstat_record(record: &[u8]) -> Option<(Option<u32>, Option<u32>, &[u8])> {
    let t1 = record.iter().position(|&b| b == b'\t')?;
    let t2 = t1 + 1 + record[t1 + 1..].iter().position(|&b| b == b'\t')?;
    let added = parse_count(&record[..t1]);
    let deleted = parse_count(&record[t1 + 1..t2]);
    Some((added, deleted, &record[t2 + 1..]))
}

/// One numstat count: a decimal, or `-` for binary.
fn parse_count(field: &[u8]) -> Option<u32> {
    if field == b"-" {
        return None;
    }
    std::str::from_utf8(field).ok()?.parse::<u32>().ok()
}

/// Working-tree files git does not track, honouring `.gitignore`.
///
/// PRD §7.5's untracked file still gets a building; `--exclude-standard` is what
/// stops that building being one per file in `target/`.
pub fn untracked_files(repo_root: &Path) -> anyhow::Result<Vec<LogicalPath>> {
    let mut out = Vec::new();
    let run = stream_git(
        repo_root,
        &["ls-files", "-z", "--others", "--exclude-standard"],
        0,
        |record| {
            if let Some(path) = decode_path_logged(record) {
                out.push(path);
            }
            Ok(())
        },
    )?;
    run.check("ls-files --others", repo_root)?;
    out.sort();
    out.dedup();
    Ok(out)
}

/// Every file in the index, sorted by logical path.
pub fn tracked_files(repo_root: &Path) -> anyhow::Result<Vec<LogicalPath>> {
    let mut out = Vec::new();
    let run = stream_git(repo_root, &["ls-files", "-z"], 0, |record| {
        if let Some(path) = decode_path_logged(record) {
            out.push(path);
        }
        Ok(())
    })?;
    run.check("ls-files", repo_root)?;
    out.sort();
    out.dedup();
    Ok(out)
}

/// Counts the lines of an untracked file, the way git would if it knew about it.
///
/// Binary detection matches git's own heuristic: a `NUL` in the first 8 000
/// bytes. A final line with no trailing newline still counts, because that is
/// what `git diff` would report once the file is added.
fn count_untracked_lines(path: &Path) -> DiffCount {
    let mut count = DiffCount {
        untracked: true,
        ..DiffCount::default()
    };
    let Ok(file) = std::fs::File::open(path) else {
        // A file `ls-files` listed and we cannot open: a symlink to nowhere, or
        // one an agent deleted between the two calls. No building height.
        return count;
    };
    let mut file = file;
    let mut buf = vec![0u8; 64 * 1024];
    let mut read: u64 = 0;
    let mut probed: usize = 0;
    let mut lines: u32 = 0;
    let mut last = b'\n';
    loop {
        // A read error is treated as end-of-file: a file we cannot finish
        // reading contributes the height of what we did read, never a failure.
        let n = file.read(&mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        if probed < BINARY_PROBE_BYTES {
            let take = (BINARY_PROBE_BYTES - probed).min(n);
            if chunk[..take].contains(&0) {
                count.binary = true;
                return count;
            }
            probed += take;
        }
        let mut newlines: u32 = 0;
        for &b in chunk {
            if b == b'\n' {
                newlines = newlines.saturating_add(1);
            }
        }
        lines = lines.saturating_add(newlines);
        last = chunk[n - 1];
        read = read.saturating_add(n as u64);
        if read >= UNTRACKED_SCAN_LIMIT {
            break;
        }
    }
    if last != b'\n' {
        lines = lines.saturating_add(1);
    }
    count.added = lines;
    count
}

// ---------------------------------------------------------------------------
// Everything git knows, folded into the tree.
// ---------------------------------------------------------------------------

/// On-disk shape of the [`History`] cache.
///
/// A private type with an explicit `version`, so the format can change without
/// a stale file from an older build parsing into something plausible and wrong.
/// A version mismatch is a miss, exactly like a corrupt file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryCache {
    version: u32,
    growth: GrowthSequence,
    last_touched: Vec<(LogicalPath, WallTime)>,
}

/// Bump on any change to [`HistoryCache`]'s meaning.
const HISTORY_CACHE_VERSION: u32 = 1;

/// Where [`History::read_cached_default`] keeps a repository's derived history.
///
/// `%LOCALAPPDATA%\polis\history\<key>.json` on Windows,
/// `$XDG_STATE_HOME/polis/history/<key>.json` elsewhere — beside the corpus
/// store, and deliberately **outside the checkout**: a cache written into the
/// repository would be walked, given a building, and change the city, which is
/// the feedback loop `crate::tree::WalkExclusions` exists to close (ADR-0065).
///
/// `key` is a written-out FNV-1a of the normalised root, the same folding
/// [`worktree_id_for`] uses, so two worktrees of one repository get two caches
/// and a checkout spelled `C:\Repo` and `c:/repo/` gets one.
///
/// `None` means no environment variable identified a state directory; the
/// caller falls back to an uncached read rather than inventing a path.
#[must_use]
pub fn default_cache_path(repo_root: &Path) -> Option<PathBuf> {
    let key = fnv1a64(normalize_root(&repo_root.to_string_lossy()).as_bytes());
    Some(
        crate::corpus::state_dir()?
            .join("history")
            .join(format!("{key:016x}.json")),
    )
}

/// Writes `bytes` to `path` via a temp file and a rename.
///
/// The pid keeps two Polis instances on one repository from writing the same
/// temp file. It never reaches the file contents, so it is not a determinism
/// hazard.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// The two history facts every building needs: when it appeared and when it was
/// last touched (PRD §7.1, §8).
#[derive(Debug, Clone, Default)]
pub struct History {
    /// Growth order — the index into this is [`crate::FileMeta::growth_index`].
    pub growth: GrowthSequence,
    /// Newest commit time per path, for PRD §8's 90-day overgrowth.
    pub last_touched: BTreeMap<LogicalPath, WallTime>,
}

impl History {
    /// Reads both from git, in **one** `git log --name-status` pass.
    ///
    /// # Why this used to be two passes, and why one is now correct
    ///
    /// The growth sequence used to be derived from PRD §7.1's pinned
    /// `--diff-filter=A` command and nothing else, so that the city's most
    /// load-bearing input had exactly one derivation; `last_touched` came from a
    /// second, unfiltered walk. The cost of that principle is a *whole extra
    /// traversal of the entire history*, and it is not small: measured on this
    /// machine, one full walk is 0.90 s on Django (34 898 commits) and 0.95 s on
    /// Neovim (37 934), so the second pass alone was a third of PRD §13.1's
    /// entire 3 s cold-start budget.
    ///
    /// `--diff-filter=A` selects entries whose status letter is `A`, and
    /// `--name-status` *prints* that letter. The two derivations are the same
    /// selection expressed twice, so folding them costs no fidelity — and the
    /// "two code paths that must agree forever" objection is answered by
    /// [`GrowthSequence::bootstrap`] remaining the PRD's pinned command and by
    /// `the_fused_walk_agrees_with_the_two_pinned_commands` asserting, on every
    /// fixture in this module, that the fused walk reproduces both of them
    /// exactly.
    pub fn read(repo_root: &Path) -> anyhow::Result<Self> {
        let head = head_commit(repo_root)?;
        let mut history = Self {
            growth: GrowthSequence {
                entries: Vec::new(),
                head,
            },
            last_touched: BTreeMap::new(),
        };
        if history.growth.head.is_empty() {
            return Ok(history);
        }
        let rev = history.growth.head.clone();
        let mut seen = BTreeSet::new();
        history.fold_range(repo_root, &rev, &mut seen)?;
        Ok(history)
    }

    /// Folds one revision range into both halves.
    ///
    /// Forward order (`--reverse`) means a later commit simply overwrites an
    /// earlier `last_touched`, and `seen` keeps a re-added path at its *first*
    /// addition, so applying a range on top of an existing history is correct
    /// without any comparison.
    fn fold_range(
        &mut self,
        repo_root: &Path,
        rev: &str,
        seen: &mut BTreeSet<LogicalPath>,
    ) -> anyhow::Result<()> {
        let growth = &mut self.growth.entries;
        let touched = &mut self.last_touched;
        let run = walk_log_status(repo_root, rev, |time, status, raw| {
            let Some(path) = decode_path_logged(raw) else {
                return;
            };
            if status == b'A' && seen.insert(path.clone()) {
                growth.push((path.clone(), time));
            }
            touched.insert(path, time);
        })?;
        run.check("log --name-status", repo_root)
    }

    /// [`Self::read`], reusing a cache keyed on `HEAD` and refreshing it
    /// incrementally when `HEAD` has moved (PRD §7.1).
    ///
    /// > Cache the derived growth sequence keyed on `HEAD`; recompute
    /// > incrementally on new commits. (PRD §7.1)
    ///
    /// **Both** halves are cached. The growth sequence alone used to be, on the
    /// reasoning that `last_touched` is invalidated broadly by any new commit —
    /// true, and beside the point: the launch that follows *no* new commit is
    /// the common one, and it was paying a full history walk for a map that had
    /// not changed. Caching one half of a pair that is always read together
    /// halves a cost that should be zero.
    ///
    /// The cache is written back on any change. A write failure is logged and
    /// swallowed: a read-only cache directory must cost a slower launch, not a
    /// failed one.
    pub fn read_cached(repo_root: &Path, cache: &Path) -> anyhow::Result<Self> {
        let head = head_commit(repo_root)?;
        if let Some(hit) = Self::load_cached(cache, &head) {
            return Ok(hit);
        }
        // A cache keyed on a *different* `HEAD` is still worth having: the whole
        // point of §7.4's incremental growth is that yesterday's history plus
        // today's commits beats recomputing yesterday.
        let history = match Self::load_stale(cache) {
            Some(mut stale) => {
                stale.extend_to_head(repo_root)?;
                stale
            }
            None => Self::read(repo_root)?,
        };
        if let Err(e) = history.save_cached(cache) {
            tracing::warn!(cache = %cache.display(), error = %e, "history cache not written");
        }
        Ok(history)
    }

    /// [`Self::read_cached`] at [`default_cache_path`] — the product path.
    ///
    /// Falls back to an uncached [`Self::read`] when no environment variable
    /// identifies a state directory, which is a slower launch and never a
    /// failure.
    pub fn read_cached_default(repo_root: &Path) -> anyhow::Result<Self> {
        match default_cache_path(repo_root) {
            Some(path) => Self::read_cached(repo_root, &path),
            None => Self::read(repo_root),
        }
    }

    /// Loads a cache whose `HEAD` still matches.
    ///
    /// Every failure — absent, unreadable, truncated, written by an older
    /// format version, or keyed on a different `HEAD` — is a **miss**, not an
    /// error. A cache that can fail a launch is worse than no cache.
    #[must_use]
    pub fn load_cached(cache: &Path, head: &str) -> Option<Self> {
        let history = Self::load_stale(cache)?;
        (history.growth.head == head).then_some(history)
    }

    /// Loads a cache whatever `HEAD` it was keyed on, for [`Self::read_cached`]
    /// to extend. A corrupt or foreign file is `None`, never an error.
    fn load_stale(cache: &Path) -> Option<Self> {
        let bytes = std::fs::read(cache).ok()?;
        let disk: HistoryCache = serde_json::from_slice(&bytes).ok()?;
        if disk.version != HISTORY_CACHE_VERSION {
            return None;
        }
        Some(Self {
            growth: disk.growth,
            last_touched: disk.last_touched.into_iter().collect(),
        })
    }

    /// Writes the cache atomically (temp + rename).
    ///
    /// `last_touched` is stored as an **array of pairs** rather than a JSON
    /// object: a `BTreeMap` keyed on [`LogicalPath`] would serialise as an
    /// object whose keys are the paths, and a path containing a character JSON
    /// has to escape is then a round-trip that depends on the encoder. The
    /// array is the same bytes on every platform, which is what makes the cache
    /// safe for a layout that must be byte-identical across machines (PRD §7.4).
    pub fn save_cached(&self, cache: &Path) -> std::io::Result<()> {
        let disk = HistoryCache {
            version: HISTORY_CACHE_VERSION,
            growth: self.growth.clone(),
            last_touched: self
                .last_touched
                .iter()
                .map(|(p, t)| (p.clone(), *t))
                .collect(),
        };
        let bytes = serde_json::to_vec(&disk).map_err(std::io::Error::other)?;
        write_atomic(cache, &bytes)
    }

    /// Brings both halves up to the current `HEAD` without a full recompute
    /// (PRD §7.4). Returns how many files entered the growth sequence.
    ///
    /// # When the promise cannot be kept
    ///
    /// A rebase, an amend, or a reset moves `HEAD` somewhere that is not a
    /// descendant of the cached one, and the history the city was built from no
    /// longer exists. Both halves are then **rebuilt** from scratch, and growth
    /// indices may move. That is deliberate: grafting the surviving paths onto
    /// the old order would make an incrementally-updated Polis and a
    /// freshly-launched Polis disagree about the same repository, which is
    /// exactly what PRD §7.4 forbids. A rewritten history is the one case where
    /// the city legitimately changes.
    pub fn extend_to_head(&mut self, repo_root: &Path) -> anyhow::Result<usize> {
        let previous = self.growth.head.clone();
        let head = head_commit(repo_root)?;
        if head == previous {
            return Ok(0);
        }
        if head.is_empty() {
            // The repository was rewound to an unborn HEAD. Nothing is added.
            self.growth.entries.clear();
            self.last_touched.clear();
            self.growth.head = head;
            return Ok(0);
        }
        if previous.is_empty() || !is_ancestor(repo_root, &previous, &head)? {
            let before = self.growth.entries.len();
            *self = Self::read(repo_root)?;
            return Ok(self.growth.entries.len().saturating_sub(before));
        }
        let range = format!("{previous}..{head}");
        let before = self.growth.entries.len();
        let mut seen: BTreeSet<LogicalPath> =
            self.growth.entries.iter().map(|(p, _)| p.clone()).collect();
        self.fold_range(repo_root, &range, &mut seen)?;
        self.growth.head = head;
        Ok(self.growth.entries.len() - before)
    }

    /// Fills [`crate::FileMeta::growth_index`], `added_at` and `last_touched` on
    /// every file the tree already holds, and sets [`RepoTree::head`].
    ///
    /// Returns how many files git had a first-addition for. Files it did not —
    /// untracked ones, and files whose only history is behind a shallow clone's
    /// graft — keep their [`crate::FileMeta::untracked`] defaults and still get a
    /// building.
    ///
    /// Adds nothing to the tree: which files exist is the walk's decision
    /// ([`crate::tree::walk`]), and a path git remembers but the filesystem no
    /// longer has is PRD §7.5's vacant lot, which the layout owns.
    pub fn apply(&self, tree: &mut RepoTree) -> usize {
        let index = self.growth.index_map();
        let mut matched = 0;
        for (path, meta) in &mut tree.files {
            if let Some(&i) = index.get(path) {
                if let Some((_, added)) = self.growth.entries.get(i as usize) {
                    meta.growth_index = i;
                    meta.added_at = *added;
                    matched += 1;
                }
            }
            if let Some(&touched) = self.last_touched.get(path) {
                meta.last_touched = touched;
            } else if meta.is_tracked() {
                // Added and never modified since.
                meta.last_touched = meta.added_at;
            }
        }
        tree.head.clone_from(&self.growth.head);
        matched
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    // -----------------------------------------------------------------------
    // Fixtures. Every repository here is created under the scratchpad by
    // `tempfile`; nothing in this module ever runs git against a real checkout.
    // -----------------------------------------------------------------------

    /// A throwaway repository with a pinned identity, pinned dates and no
    /// inherited configuration that could change what git prints.
    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let f = Self { dir };
            f.git(&["init", "-q", "-b", "main", "."]);
            f.git(&["config", "user.email", "polis@example.invalid"]);
            f.git(&["config", "user.name", "Polis Test"]);
            f.git(&["config", "commit.gpgsign", "false"]);
            f.git(&["config", "core.autocrlf", "false"]);
            f.git(&["config", "core.safecrlf", "false"]);
            f
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        /// Runs git and panics with its stderr on failure — a broken fixture
        /// must be loud, not a silently-skipped test.
        fn git(&self, args: &[&str]) -> String {
            let out = Command::new("git")
                .arg("-C")
                .arg(self.root())
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "0")
                .output()
                .expect("git must be installed to run polis-repo's tests");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        }

        fn write(&self, rel: &str, body: &str) {
            let p = self.root().join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(&p, body).expect("write");
        }

        fn write_bytes(&self, rel: &str, body: &[u8]) {
            let p = self.root().join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(&p, body).expect("write");
        }

        /// Commits everything staged at a fixed timestamp, so assertions can
        /// name exact `WallTime`s.
        fn commit_at(&self, message: &str, unix_seconds: i64) {
            let stamp = format!("@{unix_seconds} +0000");
            let out = Command::new("git")
                .arg("-C")
                .arg(self.root())
                .args(["commit", "-q", "-m", message])
                .env("GIT_AUTHOR_DATE", &stamp)
                .env("GIT_COMMITTER_DATE", &stamp)
                .output()
                .expect("git commit");
            assert!(
                out.status.success(),
                "commit failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        fn add_all_and_commit(&self, message: &str, unix_seconds: i64) {
            self.git(&["add", "-A"]);
            self.commit_at(message, unix_seconds);
        }
    }

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    fn paths(seq: &GrowthSequence) -> Vec<&str> {
        seq.entries.iter().map(|(p, _)| p.as_str()).collect()
    }

    /// The canonical fixture history, referenced by several tests.
    ///
    /// ```text
    /// t=1000  add src/a.rs, src/b.rs, docs/readme.md
    /// t=2000  add src/c.rs
    /// t=3000  modify src/a.rs
    /// t=4000  rename src/b.rs -> src/renamed.rs
    /// ```
    fn canonical() -> Fixture {
        let f = Fixture::new();
        f.write("src/a.rs", "one\n");
        f.write("src/b.rs", "two\n");
        f.write("docs/readme.md", "hello\n");
        f.add_all_and_commit("c1", 1000);

        f.write("src/c.rs", "three\n");
        f.add_all_and_commit("c2", 2000);

        f.write("src/a.rs", "one\nedited\n");
        f.add_all_and_commit("c3", 3000);

        f.git(&["mv", "src/b.rs", "src/renamed.rs"]);
        f.add_all_and_commit("c4", 4000);
        f
    }

    // -----------------------------------------------------------------------
    // Growth order — the load-bearing assertion.
    // -----------------------------------------------------------------------

    #[test]
    fn growth_order_is_commit_order_with_exact_times() {
        let f = canonical();
        let seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap");

        // Within one commit git emits paths in its own (byte) order; across
        // commits the order is history. Both are asserted exactly.
        assert_eq!(
            paths(&seq),
            [
                "docs/readme.md",
                "src/a.rs",
                "src/b.rs",
                "src/c.rs",
                "src/renamed.rs"
            ],
            "growth order is git's, oldest commit first"
        );
        let times: Vec<i64> = seq.entries.iter().map(|(_, t)| t.unix_seconds()).collect();
        assert_eq!(times, [1000, 1000, 1000, 2000, 4000]);

        assert_eq!(seq.index_of(&lp("docs/readme.md")), Some(0));
        assert_eq!(seq.index_of(&lp("src/c.rs")), Some(3));
        assert_eq!(seq.index_of(&lp("nope.rs")), None);
        assert_eq!(
            seq.added_at(&lp("src/c.rs")).map(WallTime::unix_seconds),
            Some(2000)
        );
        assert_eq!(seq.head, head_commit(f.root()).unwrap());
        assert!(!seq.is_unborn());
    }

    #[test]
    fn a_rename_is_a_new_address_and_the_old_one_keeps_its_index() {
        let f = canonical();
        let seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap");

        // Rename detection is off (PRD §7.4): it is a heuristic parameterised by
        // user config, and two operators must not get two cities.
        assert_eq!(
            seq.index_of(&lp("src/b.rs")),
            Some(2),
            "the old path keeps the position it was founded at — PRD §7.5's vacant lot"
        );
        assert_eq!(
            seq.index_of(&lp("src/renamed.rs")),
            Some(4),
            "the new path is founded at the rename commit"
        );
        assert_eq!(
            seq.added_at(&lp("src/renamed.rs"))
                .map(WallTime::unix_seconds),
            Some(4000)
        );

        // And with git's default (renames ON) the new path would have no entry
        // at all: this is what the pin buys.
        let with_detection = Command::new("git")
            .arg("-C")
            .arg(f.root())
            .args([
                "log",
                "--diff-filter=A",
                "--name-only",
                "--reverse",
                "--format=%H|%ct",
            ])
            .output()
            .expect("git log");
        let text = String::from_utf8_lossy(&with_detection.stdout);
        assert!(
            !text.contains("src/renamed.rs"),
            "sanity: with rename detection on, the renamed file never appears as added"
        );
    }

    #[test]
    fn modifying_a_file_does_not_move_it() {
        let f = canonical();
        let seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
        assert_eq!(seq.index_of(&lp("src/a.rs")), Some(1));
        assert_eq!(
            seq.added_at(&lp("src/a.rs")).map(WallTime::unix_seconds),
            Some(1000),
            "added_at is the first addition, not the last edit"
        );
    }

    #[test]
    fn a_readded_file_keeps_its_original_position() {
        let f = Fixture::new();
        f.write("a.rs", "1\n");
        f.write("z.rs", "1\n");
        f.add_all_and_commit("c1", 1000);
        std::fs::remove_file(f.root().join("a.rs")).unwrap();
        f.add_all_and_commit("c2", 2000);
        f.write("a.rs", "again\n");
        f.add_all_and_commit("c3", 3000);

        let seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
        assert_eq!(paths(&seq), ["a.rs", "z.rs"], "no duplicate entry");
        assert_eq!(seq.index_of(&lp("a.rs")), Some(0));
        assert_eq!(
            seq.added_at(&lp("a.rs")).map(WallTime::unix_seconds),
            Some(1000),
            "the address is old even though the occupant is new"
        );
    }

    #[test]
    fn non_ascii_and_spaced_paths_survive_the_walk() {
        let f = Fixture::new();
        f.write("docs/日本語.md", "こんにちは\n");
        f.write("src/имя с пробелом.rs", "x\n");
        f.write("src/emoji 🎈.rs", "y\n");
        f.add_all_and_commit("c1", 1000);

        let seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
        let got: BTreeSet<&str> = paths(&seq).into_iter().collect();
        assert!(got.contains("docs/日本語.md"), "got {got:?}");
        assert!(got.contains("src/имя с пробелом.rs"), "got {got:?}");
        assert!(got.contains("src/emoji 🎈.rs"), "got {got:?}");
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn merge_commits_add_nothing_the_branches_did_not() {
        let f = Fixture::new();
        f.write("base.rs", "b\n");
        f.add_all_and_commit("c1", 1000);
        f.git(&["checkout", "-q", "-b", "side"]);
        f.write("side.rs", "s\n");
        f.add_all_and_commit("side", 2000);
        f.git(&["checkout", "-q", "main"]);
        f.write("main.rs", "m\n");
        f.add_all_and_commit("main", 3000);
        let out = Command::new("git")
            .arg("-C")
            .arg(f.root())
            .args(["merge", "-q", "--no-ff", "-m", "merge", "side"])
            .env("GIT_AUTHOR_DATE", "@4000 +0000")
            .env("GIT_COMMITTER_DATE", "@4000 +0000")
            .output()
            .expect("merge");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
        assert_eq!(
            paths(&seq),
            ["base.rs", "side.rs", "main.rs"],
            "a merge contributes no additions of its own, and --date-order keeps \
             each branch's commits in their own commit-time order"
        );
        let times: Vec<i64> = seq.entries.iter().map(|(_, t)| t.unix_seconds()).collect();
        assert_eq!(
            times,
            [1000, 2000, 3000],
            "no path is stamped with the merge's time"
        );
    }

    #[test]
    fn a_repo_with_exactly_one_commit_lists_its_whole_tree() {
        let f = Fixture::new();
        f.write("a.rs", "1\n");
        f.write("deep/nested/b.rs", "2\n");
        f.add_all_and_commit("only", 1000);

        let seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
        assert_eq!(paths(&seq), ["a.rs", "deep/nested/b.rs"]);
        assert_eq!(seq.len(), 2);
    }

    #[test]
    fn an_empty_repo_is_an_empty_sequence_not_an_error() {
        let f = Fixture::new();
        assert_eq!(head_commit(f.root()).expect("head"), UNBORN_HEAD);
        assert_eq!(head_commit_opt(f.root()).expect("head"), None);

        let seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap on empty repo");
        assert!(seq.is_empty());
        assert!(seq.is_unborn());
        assert!(last_touched(f.root()).expect("last_touched").is_empty());

        // And the first commit is picked up incrementally from that state.
        f.write("a.rs", "1\n");
        f.add_all_and_commit("first", 1000);
        let mut seq = seq;
        assert_eq!(seq.extend_to_head(f.root()).expect("extend"), 1);
        assert_eq!(paths(&seq), ["a.rs"]);
    }

    #[test]
    fn a_shallow_clone_reads_its_graft_as_the_old_town() {
        let f = canonical();
        let url = format!(
            "file:///{}",
            f.root().display().to_string().replace('\\', "/")
        );
        let dest = tempfile::tempdir().expect("tempdir");
        let clone = dest.path().join("shallow");
        let out = Command::new("git")
            .args(["clone", "-q", "--depth", "1"])
            .arg(&url)
            .arg(&clone)
            .output()
            .expect("git clone");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        assert!(is_shallow(&clone).expect("is_shallow"));
        let seq = GrowthSequence::bootstrap(&clone).expect("bootstrap");
        // The graft is parentless, so its entire tree is "added" at the graft's
        // time. Nothing is lost and nothing panics; the history is simply flat.
        assert_eq!(
            paths(&seq),
            ["docs/readme.md", "src/a.rs", "src/c.rs", "src/renamed.rs"]
        );
        assert!(
            seq.entries.iter().all(|(_, t)| t.unix_seconds() == 4000),
            "every file dates from the graft commit"
        );
        assert!(!is_shallow(f.root()).expect("is_shallow"));
    }

    #[test]
    fn the_a_filtered_walk_agrees_with_an_unfiltered_one() {
        // `History::read` derives growth and last-touched from two separate git
        // passes. This pins the invariant that lets them coexist: a path's first
        // appearance in the unfiltered walk is its addition.
        let f = canonical();
        let seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
        let head = head_commit(f.root()).unwrap();

        let mut first_seen: Vec<LogicalPath> = Vec::new();
        let mut seen: BTreeSet<LogicalPath> = BTreeSet::new();
        walk_log(f.root(), &head, &[], |_, raw| {
            if let Some(p) = decode_path(raw) {
                if seen.insert(p.clone()) {
                    first_seen.push(p);
                }
            }
        })
        .expect("walk");

        let from_filter: Vec<LogicalPath> = seq.entries.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(from_filter, first_seen);
    }

    // -----------------------------------------------------------------------
    // The cache.
    // -----------------------------------------------------------------------

    #[test]
    fn the_cache_round_trips_byte_identically() {
        let f = canonical();
        let seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
        let cache = f.root().join(".polis/growth.json");

        seq.save_cached(&cache).expect("save");
        let first = std::fs::read(&cache).expect("read");

        let loaded = GrowthSequence::load_cached(&cache, &seq.head).expect("cache hit");
        assert_eq!(paths(&loaded), paths(&seq));
        assert_eq!(loaded.head, seq.head);

        loaded.save_cached(&cache).expect("save again");
        let second = std::fs::read(&cache).expect("read");
        assert_eq!(first, second, "the serialisation must be byte-stable");

        // And a third write from the same in-memory value, to catch any hash-map
        // iteration order that might have crept in.
        seq.save_cached(&cache).expect("save third");
        assert_eq!(first, std::fs::read(&cache).expect("read"));
    }

    #[test]
    fn a_head_mismatch_or_a_corrupt_cache_is_a_miss_not_an_error() {
        let f = canonical();
        let seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
        let cache = f.root().join("growth.json");
        seq.save_cached(&cache).expect("save");

        assert!(GrowthSequence::load_cached(&cache, "deadbeef").is_none());
        assert!(GrowthSequence::load_cached(&cache, &seq.head).is_some());

        std::fs::write(&cache, b"{\"entries\": [[\"a.rs\"").expect("truncate");
        assert!(GrowthSequence::load_cached(&cache, &seq.head).is_none());

        assert!(GrowthSequence::load_cached(&f.root().join("absent.json"), "x").is_none());
    }

    #[test]
    fn the_cache_write_is_atomic_and_leaves_no_temp_behind() {
        let f = canonical();
        let seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
        let dir = f.root().join("cachedir");
        let cache = dir.join("growth.json");
        seq.save_cached(&cache).expect("save creates its directory");
        seq.save_cached(&cache).expect("save over an existing file");

        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .expect("read_dir")
            .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
            .filter(|n| Path::new(n).extension().is_some_and(|e| e == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    // -----------------------------------------------------------------------
    // Incremental extension.
    // -----------------------------------------------------------------------

    #[test]
    fn extending_appends_and_never_reorders() {
        let f = canonical();
        let mut seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
        let before = paths(&seq)
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            seq.extend_to_head(f.root()).expect("extend"),
            0,
            "an unchanged HEAD does no work"
        );

        f.write("src/new.rs", "n\n");
        f.write("src/a.rs", "one\nedited\nagain\n");
        f.add_all_and_commit("c5", 5000);

        assert_eq!(seq.extend_to_head(f.root()).expect("extend"), 1);
        let after = paths(&seq);
        assert_eq!(&after[..before.len()], &before[..], "prefix is untouched");
        assert_eq!(after.last(), Some(&"src/new.rs"));
        assert_eq!(seq.head, head_commit(f.root()).unwrap());

        // The incrementally-grown sequence must equal a fresh bootstrap, or a
        // long-running Polis and a freshly-launched one show different cities.
        let fresh = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
        assert_eq!(paths(&seq), paths(&fresh));
        assert_eq!(
            seq.entries
                .iter()
                .map(|(_, t)| t.unix_seconds())
                .collect::<Vec<_>>(),
            fresh
                .entries
                .iter()
                .map(|(_, t)| t.unix_seconds())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_rewritten_history_rebuilds_to_exactly_what_a_fresh_launch_would_see() {
        let f = canonical();
        let mut seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap");

        // Reset away the last two commits and build different ones on top.
        f.git(&["reset", "-q", "--hard", "HEAD~2"]);
        f.write("src/other.rs", "o\n");
        f.add_all_and_commit("rewritten", 9000);

        seq.extend_to_head(f.root()).expect("extend after rewrite");
        let fresh = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
        assert_eq!(paths(&seq), paths(&fresh));
        assert_eq!(seq.head, fresh.head);
        assert!(
            !paths(&seq).contains(&"src/renamed.rs"),
            "the rewritten-away commit's file is gone"
        );
    }

    #[test]
    fn history_extends_both_halves_incrementally() {
        let f = canonical();
        let mut history = History::read(f.root()).expect("read");
        assert_eq!(
            history.last_touched[&lp("src/a.rs")].unix_seconds(),
            3000,
            "last_touched is the newest commit that touched it"
        );

        f.write("src/a.rs", "one\nedited\nagain\n");
        f.write("src/fresh.rs", "f\n");
        f.add_all_and_commit("c5", 5000);

        assert_eq!(history.extend_to_head(f.root()).expect("extend"), 1);
        assert_eq!(history.last_touched[&lp("src/a.rs")].unix_seconds(), 5000);
        assert_eq!(
            history.last_touched[&lp("src/fresh.rs")].unix_seconds(),
            5000
        );

        let fresh = History::read(f.root()).expect("read");
        assert_eq!(history.last_touched, fresh.last_touched);
        assert_eq!(paths(&history.growth), paths(&fresh.growth));
    }

    #[test]
    fn read_cached_extends_a_stale_cache_instead_of_rebuilding() {
        let f = canonical();
        // Outside the repository: a cache written inside it would be swept up by
        // the fixture's own `git add -A` and become a commit of its own.
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let cache = cache_dir.path().join("growth.json");
        let first = History::read_cached(f.root(), &cache).expect("first");
        assert!(cache.exists(), "the cache is written on a miss");

        f.write("src/late.rs", "l\n");
        f.add_all_and_commit("c5", 5000);

        let second = History::read_cached(f.root(), &cache).expect("second");
        assert_eq!(second.growth.len(), first.growth.len() + 1);
        assert_eq!(second.growth.head, head_commit(f.root()).unwrap());
        assert_eq!(
            &paths(&second.growth)[..first.growth.len()],
            &paths(&first.growth)[..],
            "the cached prefix survived"
        );

        // Third read is a straight hit.
        let third = History::read_cached(f.root(), &cache).expect("third");
        assert_eq!(paths(&third.growth), paths(&second.growth));
        assert_eq!(third.last_touched, second.last_touched);

        // And the whole point: a cache that was *extended* must give the same
        // history a launch with no cache at all would. Anything less and an
        // incrementally-updated Polis and a freshly-launched one disagree about
        // the same repository, which is what PRD §7.4 forbids.
        let fresh = History::read(f.root()).expect("fresh");
        assert_eq!(
            third.growth.entries, fresh.growth.entries,
            "the extended cache and a cold read disagree about the growth order"
        );
        assert_eq!(third.growth.head, fresh.growth.head);
        assert_eq!(
            third.last_touched, fresh.last_touched,
            "the extended cache and a cold read disagree about last-touched"
        );
    }

    /// The fused `--name-status` walk reproduces **both** pinned commands
    /// exactly, on every shape of history this module has a fixture for.
    ///
    /// This is the test that lets [`History::read`] be one pass instead of two.
    /// The saving is real — one full walk is ~0.9 s on a repository of Django's
    /// age, a third of PRD §13.1's whole cold-start budget — but it is only
    /// safe while `status == b'A'` and `--diff-filter=A` select the same
    /// entries, and that is an assertion about git, not about this code.
    #[test]
    fn the_fused_walk_agrees_with_the_two_pinned_commands() {
        let mut checked = 0;
        for (name, f) in [
            ("canonical", canonical()),
            ("one commit", {
                let f = Fixture::new();
                f.write("only.rs", "x\n");
                f.add_all_and_commit("c1", 1000);
                f
            }),
            ("non-ascii and deletions", {
                let f = Fixture::new();
                f.write("src/héllo wörld.rs", "a\n");
                f.write("src/gone.rs", "b\n");
                f.add_all_and_commit("c1", 1000);
                std::fs::remove_file(f.root().join("src/gone.rs")).expect("rm");
                f.add_all_and_commit("c2", 2000);
                // Re-added at a new time: the growth index must stay at the
                // first addition while last-touched moves.
                f.write("src/gone.rs", "c\n");
                f.add_all_and_commit("c3", 3000);
                f
            }),
            ("unborn", Fixture::new()),
        ] {
            let fused = History::read(f.root()).expect("fused read");
            let pinned_growth = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
            let pinned_touched = last_touched_map(f.root()).expect("last_touched");
            assert_eq!(
                fused.growth.entries, pinned_growth.entries,
                "{name}: the fused growth order left --diff-filter=A behind"
            );
            assert_eq!(fused.growth.head, pinned_growth.head, "{name}: head");
            assert_eq!(
                fused.last_touched, pinned_touched,
                "{name}: the fused last-touched left the unfiltered walk behind"
            );
            checked += 1;
        }
        assert_eq!(checked, 4, "the corpus shrank");
    }

    /// The cache carries **both** halves, and anything it cannot vouch for is a
    /// miss rather than an error.
    #[test]
    fn the_history_cache_round_trips_and_anything_doubtful_is_a_miss() {
        let f = canonical();
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = dir.path().join("nested/history.json");
        let history = History::read(f.root()).expect("read");
        history.save_cached(&cache).expect("save");

        let hit = History::load_cached(&cache, &history.growth.head).expect("hit");
        assert_eq!(hit.growth.entries, history.growth.entries);
        assert_eq!(
            hit.last_touched, history.last_touched,
            "last_touched is cached too, or the second launch pays a full walk"
        );

        assert!(
            History::load_cached(&cache, "0".repeat(40).as_str()).is_none(),
            "a cache keyed on another HEAD is a miss"
        );
        assert!(
            History::load_cached(
                dir.path().join("absent.json").as_path(),
                &history.growth.head
            )
            .is_none(),
            "an absent cache is a miss, not an error"
        );

        // A file from a future format version parses as JSON and must still be
        // refused, or a stale cache would deserialise into something plausible.
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&cache).expect("read")).expect("json");
        raw["version"] = serde_json::Value::from(HISTORY_CACHE_VERSION + 1);
        std::fs::write(&cache, serde_json::to_vec(&raw).expect("json")).expect("write");
        assert!(
            History::load_cached(&cache, &history.growth.head).is_none(),
            "a foreign format version is a miss"
        );

        std::fs::write(&cache, b"{ not json").expect("write");
        assert!(History::load_cached(&cache, &history.growth.head).is_none());
        // And a corrupt cache still yields a correct history, the slow way.
        let recovered = History::read_cached(f.root(), &cache).expect("recover");
        assert_eq!(recovered.growth.entries, history.growth.entries);
        assert_eq!(recovered.last_touched, history.last_touched);
    }

    /// The default cache lives outside the checkout and folds the spelling of
    /// the root, so `C:\Repo` and `c:/repo/` share one file (ADR-0065).
    #[test]
    fn the_default_cache_path_is_outside_the_checkout() {
        let f = canonical();
        let Some(path) = default_cache_path(f.root()) else {
            // No state directory on this machine: the caller falls back to an
            // uncached read, which is the documented degradation.
            return;
        };
        assert!(
            !path.starts_with(f.root()),
            "the cache is inside the repository it describes: {}",
            path.display()
        );
        let spelled = PathBuf::from(f.root().to_string_lossy().replace('\\', "/") + "/");
        assert_eq!(
            default_cache_path(&spelled),
            Some(path),
            "two spellings of one root took two caches"
        );
    }

    // -----------------------------------------------------------------------
    // last_touched.
    // -----------------------------------------------------------------------

    #[test]
    fn last_touched_is_the_newest_commit_per_path_including_deletions() {
        let f = canonical();
        std::fs::remove_file(f.root().join("src/c.rs")).unwrap();
        f.add_all_and_commit("delete c", 6000);

        let map: BTreeMap<LogicalPath, WallTime> = last_touched(f.root())
            .expect("last_touched")
            .into_iter()
            .collect();
        assert_eq!(map[&lp("docs/readme.md")].unix_seconds(), 1000);
        assert_eq!(map[&lp("src/a.rs")].unix_seconds(), 3000);
        assert_eq!(
            map[&lp("src/b.rs")].unix_seconds(),
            4000,
            "deleted by the rename"
        );
        assert_eq!(map[&lp("src/renamed.rs")].unix_seconds(), 4000);
        assert_eq!(
            map[&lp("src/c.rs")].unix_seconds(),
            6000,
            "a vacant lot ages from its deletion"
        );

        // Sorted by path, so the Vec form is deterministic.
        let v = last_touched(f.root()).expect("last_touched");
        let mut sorted = v.clone();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(v, sorted);
    }

    // -----------------------------------------------------------------------
    // numstat.
    // -----------------------------------------------------------------------

    #[test]
    // One git worktree carried through every numstat state in sequence — staged,
    // unstaged, untracked, binary, deleted. Splitting it into five tests would
    // mean five fixture repositories and five replays of the same history, and
    // would lose the thing it actually asserts: that the states are read
    // correctly *together*, in one working tree, which is the only way they ever
    // occur. (Tripped only after `cargo fmt` re-wrapped the assertions.)
    #[allow(clippy::too_many_lines)]
    fn numstat_covers_staged_unstaged_untracked_and_binary() {
        let f = Fixture::new();
        f.write("keep.rs", "1\n2\n3\n");
        f.write("staged.rs", "a\n");
        f.write("both.rs", "x\n");
        f.add_all_and_commit("c1", 1000);

        // Staged only.
        f.write("staged.rs", "a\nb\nc\n");
        f.git(&["add", "staged.rs"]);
        // Staged, then edited again *over* the staged line. Against HEAD this is
        // one added line; summing `git diff` and `git diff --cached` would report
        // two added and one deleted. The two answers differ, which is the point.
        f.write("both.rs", "x\ny\n");
        f.git(&["add", "both.rs"]);
        f.write("both.rs", "x\nz\n");
        // Unstaged only.
        f.write("keep.rs", "1\n2\n");
        // A new binary file, staged.
        f.write_bytes("blob.bin", &[0u8, 1, 2, 3, 0, 9]);
        f.git(&["add", "blob.bin"]);
        // Untracked, one with no trailing newline.
        f.write("untracked.rs", "u1\nu2\nu3\n");
        f.write("no-newline.rs", "only");
        f.write_bytes("untracked.bin", &[1u8, 2, 0, 3]);
        // Ignored files get no building.
        f.write(".gitignore", "ignored.log\n");
        f.write("ignored.log", "noise\nnoise\n");

        let counts = diff_counts(f.root()).expect("diff_counts");

        assert_eq!(
            counts[&lp("staged.rs")],
            DiffCount {
                added: 2,
                deleted: 0,
                binary: false,
                untracked: false
            },
            "a staged change counts even though it is not in the working-tree diff"
        );
        assert_eq!(
            counts[&lp("both.rs")],
            DiffCount {
                added: 1,
                deleted: 0,
                binary: false,
                untracked: false
            },
            "staged + unstaged is counted once against HEAD, not summed twice \
             (the naive sum would be 2 added and 1 deleted)"
        );
        assert_eq!(
            counts[&lp("keep.rs")],
            DiffCount {
                added: 0,
                deleted: 1,
                binary: false,
                untracked: false
            }
        );
        assert_eq!(
            counts[&lp("blob.bin")],
            DiffCount {
                added: 0,
                deleted: 0,
                binary: true,
                untracked: false
            },
            "git reports `-` for both counts on a binary file"
        );
        assert_eq!(
            counts[&lp("untracked.rs")],
            DiffCount {
                added: 3,
                deleted: 0,
                binary: false,
                untracked: true
            }
        );
        assert_eq!(
            counts[&lp("no-newline.rs")],
            DiffCount {
                added: 1,
                deleted: 0,
                binary: false,
                untracked: true
            },
            "a final line without a newline is still a line"
        );
        assert_eq!(
            counts[&lp("untracked.bin")],
            DiffCount {
                added: 0,
                deleted: 0,
                binary: true,
                untracked: true
            }
        );
        assert!(
            !counts.contains_key(&lp("ignored.log")),
            "gitignore is honoured"
        );

        // The public flattening.
        let flat: BTreeMap<LogicalPath, u32> = diff_line_counts(f.root())
            .expect("counts")
            .into_iter()
            .collect();
        assert_eq!(flat[&lp("keep.rs")], 1, "height is added + deleted");
        assert_eq!(flat[&lp("staged.rs")], 2);
        assert_eq!(flat[&lp("blob.bin")], 0);

        assert!(DiffCount::default().is_empty());
        assert!(
            !counts[&lp("blob.bin")].is_empty(),
            "a binary change is still a change"
        );
    }

    #[test]
    fn numstat_on_a_repo_with_no_commits_uses_the_index() {
        let f = Fixture::new();
        f.write("a.rs", "1\n2\n");
        f.git(&["add", "a.rs"]);
        f.write("b.rs", "3\n");

        let counts = diff_counts(f.root()).expect("diff_counts on an unborn HEAD");
        assert_eq!(counts[&lp("a.rs")].added, 2);
        assert_eq!(counts[&lp("b.rs")].added, 1);
        assert!(counts[&lp("b.rs")].untracked);
    }

    #[test]
    fn numstat_record_parsing_handles_binary_and_the_rename_form() {
        assert_eq!(
            parse_numstat_record(b"12\t3\tsrc/a.rs").map(|(a, d, p)| (a, d, p.to_vec())),
            Some((Some(12), Some(3), b"src/a.rs".to_vec()))
        );
        assert_eq!(
            parse_numstat_record(b"-\t-\tblob.bin").map(|(a, d, p)| (a, d, p.to_vec())),
            Some((None, None, b"blob.bin".to_vec()))
        );
        // The `-z` rename form has an empty path; the two following records are
        // the old and new names.
        let (a, d, p) = parse_numstat_record(b"5\t5\t").expect("rename header");
        assert_eq!((a, d), (Some(5), Some(5)));
        assert!(p.is_empty());
        // A path containing a tab arrives raw under `-z` and must not be split.
        assert_eq!(
            parse_numstat_record(b"1\t0\tsrc/a\tb.rs").map(|(_, _, p)| p.to_vec()),
            Some(b"src/a\tb.rs".to_vec())
        );
        assert!(parse_numstat_record(b"garbage").is_none());
        assert!(parse_numstat_record(b"1\tnotanumber\tx.rs").is_some());
        assert_eq!(parse_count(b"-"), None);
        assert_eq!(parse_count(b"0"), Some(0));
        assert_eq!(parse_count(b"4294967296"), None, "overflow is not a panic");
    }

    #[test]
    fn tracked_and_untracked_listings_are_sorted_and_disjoint() {
        let f = canonical();
        f.write("zz.rs", "z\n");
        f.write("aa.rs", "a\n");
        let tracked = tracked_files(f.root()).expect("tracked");
        let untracked = untracked_files(f.root()).expect("untracked");

        let mut sorted = tracked.clone();
        sorted.sort();
        assert_eq!(tracked, sorted);
        assert_eq!(untracked, vec![lp("aa.rs"), lp("zz.rs")]);
        for u in &untracked {
            assert!(!tracked.contains(u));
        }
    }

    // -----------------------------------------------------------------------
    // C-style quoting.
    // -----------------------------------------------------------------------

    #[test]
    fn c_style_quoting_is_reversed_including_octal_utf8() {
        // Exactly what git emits for `docs/日本語.md` with core.quotePath on.
        let quoted = br#""docs/\346\227\245\346\234\254\350\252\236.md""#;
        let out = unquote_c_style(quoted).expect("unquote");
        assert_eq!(std::str::from_utf8(&out).unwrap(), "docs/日本語.md");
        assert_eq!(
            decode_path(quoted).map(|p| p.as_str().to_owned()),
            Some("docs/日本語.md".to_owned())
        );

        // The escapes git emits regardless of core.quotePath.
        assert_eq!(unquote_c_style(br#""a\"b""#).unwrap(), b"a\"b");
        assert_eq!(unquote_c_style(br#""a\\b""#).unwrap(), b"a\\b");
        assert_eq!(unquote_c_style(br#""a\tb""#).unwrap(), b"a\tb");
        assert_eq!(unquote_c_style(br#""a\nb""#).unwrap(), b"a\nb");
        assert_eq!(unquote_c_style(br#""a\rb""#).unwrap(), b"a\rb");
        assert_eq!(unquote_c_style(br#""\a\b\f\v""#).unwrap(), [7, 8, 12, 11]);
        // Short octal runs, which git does not emit but a fixture might.
        assert_eq!(unquote_c_style(br#""\1\12\101""#).unwrap(), [1, 10, b'A']);

        // Malformed input is refused, never panicked on.
        assert!(unquote_c_style(b"\"unterminated").is_none());
        assert!(unquote_c_style(b"\"").is_none());
        assert!(unquote_c_style(b"").is_none());
        assert!(unquote_c_style(br#""trailing\"#).is_none());

        // A quoted path that decodes to invalid UTF-8 gets no building rather
        // than a lossy key that would merge two files (ADR-0028).
        assert!(decode_path(br#""bad\377name.rs""#).is_none());
        assert!(decode_path(b"").is_none());
        assert!(decode_path(b"/absolute.rs").is_none());
    }

    #[test]
    fn a_header_is_told_from_a_path_by_shape() {
        assert_eq!(
            parse_header(b"63ce289fa7e84558a7807427ab75e295cf18f25a|1577836800")
                .map(WallTime::unix_seconds),
            Some(1_577_836_800)
        );
        // SHA-256.
        let sha256 = "a".repeat(64);
        assert_eq!(
            parse_header(format!("{sha256}|42").as_bytes()).map(WallTime::unix_seconds),
            Some(42)
        );
        // A pre-1970 commit from a rewritten history stays representable.
        let sha1 = "b".repeat(40);
        assert_eq!(
            parse_header(format!("{sha1}|-86400").as_bytes()).map(WallTime::unix_seconds),
            Some(-86_400)
        );
        // Paths that could be mistaken for headers.
        assert!(parse_header(b"src/a|b.rs").is_none());
        assert!(parse_header(format!("{sha1}|notanumber").as_bytes()).is_none());
        assert!(parse_header(format!("{}|1", "z".repeat(40)).as_bytes()).is_none());
        assert!(parse_header(b"src/a.rs").is_none());
    }

    #[test]
    fn a_filename_shaped_like_a_header_is_still_a_filename() {
        // `|` is illegal in a Windows filename, so this case cannot be built as
        // a fixture repository on the machine PRD §16's golden test runs on.
        // The classifier is exercised directly instead, on exactly the bytes git
        // would emit for a repository containing such a file — which is legal on
        // ext4 and APFS, and therefore reachable.
        let sha = "a".repeat(40);
        let hostile = format!("{sha}|1000");
        let transcript: Vec<Vec<u8>> = vec![
            format!("{sha}|1000").into_bytes(),
            Vec::new(),
            hostile.clone().into_bytes(),
            b"zzz.rs".to_vec(),
            format!("{}|2000", "b".repeat(40)).into_bytes(),
            Vec::new(),
            b"later.rs".to_vec(),
        ];

        let mut expect_path = false;
        let mut seen: Vec<(i64, String)> = Vec::new();
        let mut time = 0;
        for line in &transcript {
            match classify_log_line(line, &mut expect_path) {
                LogLine::Blank => {}
                LogLine::Header(t) => time = t.unix_seconds(),
                LogLine::Path(p) => seen.push((time, String::from_utf8_lossy(p).into_owned())),
            }
        }
        assert_eq!(
            seen,
            vec![
                (1000, hostile),
                (1000, "zzz.rs".to_owned()),
                (2000, "later.rs".to_owned()),
            ],
            "the first line after a header is a path, whatever it looks like"
        );
    }

    // -----------------------------------------------------------------------
    // Worktrees.
    // -----------------------------------------------------------------------

    #[test]
    fn worktree_porcelain_is_parsed_including_detached_and_bare() {
        let text = concat!(
            "worktree /repo\nHEAD 1111111111111111111111111111111111111111\n",
            "branch refs/heads/main\n\n",
            "worktree /repo-wt-3\nHEAD 2222222222222222222222222222222222222222\n",
            "detached\n\n",
            "worktree /repo-wt-7\nHEAD 3333333333333333333333333333333333333333\n",
            "branch refs/heads/feature/x\nlocked because I said so\nprunable gitdir gone\n\n",
            "worktree /unborn\nHEAD 0000000000000000000000000000000000000000\n",
            "branch refs/heads/main\n\n",
        );
        let raw = parse_worktree_porcelain(text);
        assert_eq!(raw.len(), 4);
        assert_eq!(raw[0].branch.as_deref(), Some("main"));
        assert_eq!(raw[1].branch, None, "detached carries no branch");
        assert_eq!(raw[2].branch.as_deref(), Some("feature/x"));
        assert_eq!(raw[3].head, UNBORN_HEAD, "an all-zero HEAD is unborn");

        // A bare main worktree has no HEAD line at all, and no trailing blank.
        let bare = parse_worktree_porcelain("worktree /bare\nbare\n");
        assert_eq!(bare.len(), 1);
        assert_eq!(bare[0].head, UNBORN_HEAD);
        assert_eq!(bare[0].branch, None);

        let assigned = assign_worktree_ids(raw);
        assert_eq!(assigned[0].id, WorktreeId::PRIMARY);
        assert!(assigned[1..].iter().all(|w| !w.id.is_primary()));
        let ids: BTreeSet<WorktreeId> = assigned.iter().map(|w| w.id).collect();
        assert_eq!(ids.len(), assigned.len(), "ids are distinct");
        assert!(assigned[1].is_detached());
        assert!(!assigned[0].is_detached());
        assert!(!assigned[0].same_branch_as(&assigned[1]));
        assert!(
            !assigned[1].same_branch_as(&assigned[1].clone()),
            "ADR-0018"
        );
    }

    #[test]
    fn worktree_ids_are_stable_and_path_derived() {
        assert_ne!(worktree_id_for("/repo-wt-3"), WorktreeId::PRIMARY);
        assert_eq!(worktree_id_for("/repo-wt-3"), worktree_id_for("/repo-wt-3"));
        assert_ne!(worktree_id_for("/repo-wt-3"), worktree_id_for("/repo-wt-7"));
        // Spelling does not change identity: separators, trailing slash and
        // ASCII case all fold, exactly as LogicalPath folds them (ADR-0028).
        assert_eq!(
            worktree_id_for(r"C:\repo\wt"),
            worktree_id_for("c:/REPO/wt/")
        );
        assert_eq!(
            worktree_id_for(r"\\?\C:\repo\wt"),
            worktree_id_for("C:/repo/wt")
        );
        // Pinned, because this value reaches the screen as a tint and must not
        // move when the toolchain does (ADR-0029).
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"polis"), 0x90bb_1735_55d1_cb0c);
    }

    #[test]
    fn a_real_worktree_maps_to_the_same_logical_path_as_the_main_checkout() {
        let f = canonical();
        let sibling = tempfile::tempdir().expect("tempdir");
        let wt = sibling.path().join("repo-wt-3");
        let detached = sibling.path().join("repo-wt-7");
        f.git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "agent-3",
            wt.to_str().unwrap(),
        ]);
        f.git(&[
            "worktree",
            "add",
            "-q",
            "--detach",
            detached.to_str().unwrap(),
        ]);

        let set = WorktreeSet::discover(f.root()).expect("discover");
        assert_eq!(set.worktrees().len(), 3);
        assert_eq!(set.primary().id, WorktreeId::PRIMARY);
        assert_eq!(set.primary().branch.as_deref(), Some("main"));

        let linked: Vec<&Worktree> = set
            .worktrees()
            .iter()
            .filter(|w| !w.id.is_primary())
            .collect();
        let branched = linked
            .iter()
            .find(|w| w.branch.as_deref() == Some("agent-3"))
            .expect("the branched worktree");
        let head_less = linked.iter().find(|w| w.is_detached()).expect("detached");
        assert_eq!(head_less.head, f.git(&["rev-parse", "HEAD"]).trim());
        assert_eq!(set.branch(branched.id), Some("agent-3"));
        assert_eq!(
            set.branch(head_less.id),
            None,
            "a detached HEAD is not a branch"
        );

        // PRD §7.6: the same logical file in two physical places.
        let main_hit = set
            .to_logical(&f.root().join("src/a.rs"))
            .expect("main checkout");
        let wt_hit = set
            .to_logical(&wt.join("src/a.rs"))
            .expect("linked checkout");
        assert_eq!(main_hit.1, lp("src/a.rs"));
        assert_eq!(wt_hit.1, lp("src/a.rs"), "the worktree prefix is stripped");
        assert_eq!(main_hit.0, WorktreeId::PRIMARY);
        assert_eq!(wt_hit.0, branched.id);
        assert_ne!(main_hit.0, wt_hit.0, "the checkout is a separate dimension");

        // Relative paths resolve against a cwd, which is how tool inputs arrive.
        let rel = set
            .resolve(Some(&wt), "src/a.rs")
            .expect("relative to the worktree");
        assert_eq!(rel, wt_hit);

        // Outside every checkout is None, not an error.
        assert!(set
            .to_logical(Path::new("/definitely/elsewhere/x.rs"))
            .is_none());

        // And the tree's checkout dimension.
        let mut tree = RepoTree::default();
        set.apply(&mut tree);
        assert_eq!(tree.root, f.root());
        assert_eq!(tree.worktrees.len(), 3);
        assert_eq!(tree.worktrees[&WorktreeId::PRIMARY], f.root());
        assert_eq!(
            set.get(branched.id).map(|w| w.path.clone()),
            Some(wt.clone())
        );
        assert!(set.get(WorktreeId(u32::MAX - 1)).is_none());

        // Clean up before the TempDirs drop, or git leaves administrative files.
        f.git(&["worktree", "remove", "--force", wt.to_str().unwrap()]);
        f.git(&["worktree", "remove", "--force", detached.to_str().unwrap()]);
    }

    #[test]
    fn discovering_worktrees_twice_gives_identical_ids() {
        let f = canonical();
        let sibling = tempfile::tempdir().expect("tempdir");
        let wt = sibling.path().join("wt-a");
        f.git(&["worktree", "add", "-q", "-b", "a", wt.to_str().unwrap()]);

        let first = WorktreeSet::discover(f.root()).expect("discover");
        let second = WorktreeSet::discover(f.root()).expect("discover");
        assert_eq!(first.roots(), second.roots());

        // Adding a second worktree must not renumber the first.
        let wt2 = sibling.path().join("wt-b");
        f.git(&["worktree", "add", "-q", "-b", "b", wt2.to_str().unwrap()]);
        let third = WorktreeSet::discover(f.root()).expect("discover");
        for (id, path) in first.roots() {
            assert_eq!(third.roots().get(&id), Some(&path), "id {id} moved");
        }

        f.git(&["worktree", "remove", "--force", wt.to_str().unwrap()]);
        f.git(&["worktree", "remove", "--force", wt2.to_str().unwrap()]);
    }

    // -----------------------------------------------------------------------
    // Folding history into the tree.
    // -----------------------------------------------------------------------

    #[test]
    fn history_populates_file_meta() {
        use crate::FileMeta;

        let f = canonical();
        let history = History::read(f.root()).expect("read");

        let mut tree = RepoTree::default();
        for p in [
            "docs/readme.md",
            "src/a.rs",
            "src/c.rs",
            "src/renamed.rs",
            "scratch.rs",
        ] {
            let path = lp(p);
            tree.files
                .insert(path.clone(), FileMeta::untracked(path, 10));
        }

        let matched = history.apply(&mut tree);
        assert_eq!(matched, 4, "scratch.rs is untracked");
        assert_eq!(tree.head, head_commit(f.root()).unwrap());

        let readme = &tree.files[&lp("docs/readme.md")];
        assert_eq!(readme.growth_index, 0);
        assert_eq!(readme.added_at.unix_seconds(), 1000);
        assert_eq!(
            readme.last_touched.unix_seconds(),
            1000,
            "never modified, so last_touched is its addition"
        );
        assert!(readme.is_tracked());

        let a = &tree.files[&lp("src/a.rs")];
        assert_eq!(a.growth_index, 1);
        assert_eq!(a.added_at.unix_seconds(), 1000);
        assert_eq!(a.last_touched.unix_seconds(), 3000);

        let renamed = &tree.files[&lp("src/renamed.rs")];
        assert_eq!(renamed.growth_index, 4);
        assert_eq!(renamed.added_at.unix_seconds(), 4000);

        let scratch = &tree.files[&lp("scratch.rs")];
        assert!(!scratch.is_tracked(), "growth_index stays u32::MAX");
        assert_eq!(scratch.added_at, WallTime::UNIX_EPOCH);
        assert_eq!(scratch.last_touched, WallTime::UNIX_EPOCH);

        // Growth indices are a permutation of 0..n over the tracked files.
        let mut indices: Vec<u32> = tree
            .files
            .values()
            .filter(|m| m.is_tracked())
            .map(|m| m.growth_index)
            .collect();
        indices.sort_unstable();
        assert_eq!(indices, [0, 1, 3, 4]);

        // PRD §8's overgrowth reads the same field.
        let now = WallTime::from_unix_seconds(1000 + 91 * 86_400);
        assert!(readme.last_touched.days_until(now) >= crate::tree::OVERGROWTH_DAYS);
    }

    #[test]
    fn the_index_map_agrees_with_index_of() {
        let f = canonical();
        let seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
        let map = seq.index_map();
        assert_eq!(map.len(), seq.len());
        for (path, _) in &seq.entries {
            assert_eq!(map.get(path).copied(), seq.index_of(path));
        }
    }

    // -----------------------------------------------------------------------
    // Invocation hygiene.
    // -----------------------------------------------------------------------

    #[test]
    fn self_check_distinguishes_a_repo_from_a_bare_directory() {
        let f = canonical();
        self_check(f.root()).expect("a real repository");

        let plain = tempfile::tempdir().expect("tempdir");
        let err = self_check(plain.path()).expect_err("not a repository");
        let text = format!("{err:#}");
        assert!(
            text.contains("rev-parse") || text.contains("not a git repository"),
            "unhelpful error: {text}"
        );
    }

    #[test]
    fn an_inherited_git_dir_cannot_redirect_the_walk() {
        // A Polis launched from inside another checkout by a tool that exports
        // GIT_DIR must still read the repository it was pointed at.
        let f = canonical();
        let decoy = Fixture::new();
        decoy.write("decoy.rs", "d\n");
        decoy.add_all_and_commit("decoy", 1000);

        // SAFETY-free: this is a plain environment write in a single-threaded
        // test, but set_var is unsafe in edition 2024, so use the child process
        // path instead — stream_git strips GIT_DIR from the command's own
        // environment, which is what actually matters.
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(f.root()).args(["rev-parse", "--git-dir"]);
        cmd.env("GIT_DIR", decoy.root().join(".git"));
        let out = cmd.output().expect("git");
        let hijacked = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        assert!(
            hijacked.contains("decoy") || Path::new(&hijacked).exists(),
            "sanity: GIT_DIR does redirect a naive invocation ({hijacked})"
        );

        // And ours does not, because ENV_STRIP removes it.
        let seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
        assert!(paths(&seq).contains(&"src/a.rs"));
        assert!(!paths(&seq).contains(&"decoy.rs"));
    }

    #[test]
    fn a_verbatim_prefix_is_stripped_before_git_sees_it() {
        assert_eq!(strip_verbatim(r"\\?\C:\repo"), r"C:\repo");
        assert_eq!(strip_verbatim(r"\\?\UNC\server\share"), r"\\server\share");
        assert_eq!(strip_verbatim(r"C:\repo"), r"C:\repo");
        assert_eq!(strip_verbatim("/home/x/repo"), "/home/x/repo");
        // Non-ASCII immediately after the prefix: byte slicing, never `str`
        // indexing, or this panics (ADR-0046).
        assert_eq!(strip_verbatim(r"\\?\C:\日本\a.rs"), r"C:\日本\a.rs");
        assert_eq!(strip_verbatim(r"\\?\日本"), "日本");
    }

    #[test]
    fn a_verbatim_repo_root_still_works() {
        let f = canonical();
        let canonical_root = std::fs::canonicalize(f.root()).expect("canonicalize");
        // On Windows this is a `\\?\` path; on Unix it is unchanged. Either way
        // the walk must produce the same sequence.
        let seq = GrowthSequence::bootstrap(&canonical_root).expect("bootstrap");
        let plain = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
        assert_eq!(paths(&seq), paths(&plain));
    }

    #[test]
    fn a_large_output_streams_without_truncation() {
        // Deliberately more files than fit in a pipe buffer, so that a
        // `Command::output()`-shaped bug or a stderr deadlock shows up here
        // rather than on a real repository.
        let f = Fixture::new();
        let mut expected = Vec::new();
        for i in 0..1500 {
            let mut name = String::new();
            write!(name, "src/dir{:03}/file{:04}.rs", i % 40, i).unwrap();
            f.write(&name, "x\n");
            expected.push(name);
        }
        f.add_all_and_commit("bulk", 1000);
        expected.sort();

        let seq = GrowthSequence::bootstrap(f.root()).expect("bootstrap");
        let mut got: Vec<String> = paths(&seq).into_iter().map(str::to_owned).collect();
        got.sort();
        assert_eq!(got, expected);
    }
}
