//! The index of the operator's own sessions — the onboarding key (PRD §15 M2).
//!
//! The operator already has hundreds or thousands of real sessions in
//! `~/.claude/projects`. This module enumerates them so a first run can say
//! *"pick one of your own sessions and watch it"* with zero configuration,
//! rather than asking for a path to a file nobody knows the shape of.
//!
//! # Speed
//!
//! There are ~950 transcript files and ~775 MB in a real corpus, of which ~300 MB
//! across ~186 files are main transcripts. Three things keep this fast:
//!
//! 1. **Head and tail, not the whole file, for the metadata.** The first
//!    [`HEAD_BYTES`] carry `cwd`, `sessionId`, `version` and the first
//!    timestamp; the last [`TAIL_BYTES`] carry the last timestamp and the
//!    title. Neither needs a JSON parser — the on-disk format is compact JSON
//!    with no spaces, so the fields are found by byte search.
//! 2. **A byte scan, not a parse, for the counts.** `"type":"tool_use"` and
//!    `"file_path":"` are counted by scanning; parsing 300 MB of JSON to learn
//!    two numbers would cost tens of seconds.
//! 3. **A cache keyed on `(size, mtime)`.** Transcripts are append-only, so an
//!    unchanged file is never read twice. See [`IndexOptions::cache`].
//!
//! # Honesty
//!
//! [`SessionSummary::repo_exists`] is checked against disk, because a session
//! whose repository has been deleted or renamed cannot be replayed over a city —
//! and offering it anyway is how a first run fails in a way the operator cannot
//! diagnose. [`SessionSummary::counts_partial`] says when a file was too large
//! to scan and its counts are therefore missing rather than zero.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use polis_events::{PathMapper, SessionId, WallTime};
use polis_ingest::transcript::{discover_project_dirs, parse_timestamp, sessions_in};
use serde::{Deserialize, Serialize};

use crate::replay::ReplaySchedule;

/// How much of a transcript's head is read for metadata.
pub const HEAD_BYTES: u64 = 128 * 1024;

/// How much of a transcript's tail is read for the end time and the title.
pub const TAIL_BYTES: u64 = 128 * 1024;

/// Largest file the counting scan will read whole.
///
/// The largest main transcript measured on this machine is 23 MB; 64 MiB leaves
/// room and still refuses to page in something pathological.
pub const MAX_FULL_READ: u64 = 64 * 1024 * 1024;

/// Cache format version. A bump invalidates every cached entry, on purpose.
pub const CACHE_VERSION: u32 = 1;

/// `~/.claude/projects`, when there is a home directory to derive it from.
pub fn default_projects_dir() -> Option<PathBuf> {
    polis_ingest::default_claude_projects_dir()
}

/// Where the index cache lives — beside the PRD §6.1 corpus.
pub fn default_cache_path() -> Option<PathBuf> {
    polis_repo::corpus::Corpus::default_state_dir().map(|d| d.join("sessions-index.json"))
}

/// How thoroughly to read each transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexDepth {
    /// Head and tail only: identity, repository, start, end, size. Counts are
    /// left at zero and [`SessionSummary::counts_partial`] is set. Milliseconds
    /// for a whole corpus.
    Headers,
    /// Also scan the body for tool calls and touched files.
    #[default]
    Full,
}

/// How to run the scan.
#[derive(Debug, Clone)]
pub struct IndexOptions {
    /// Head/tail only, or a full byte scan.
    pub depth: IndexDepth,
    /// Where to read and write the `(size, mtime)`-keyed cache. `None` disables
    /// caching entirely.
    pub cache: Option<PathBuf>,
    /// Stop after this many sessions, newest file first. `None` indexes
    /// everything.
    pub limit: Option<usize>,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            depth: IndexDepth::default(),
            cache: default_cache_path(),
            limit: None,
        }
    }
}

impl IndexOptions {
    /// The fastest useful scan: head and tail, no cache write.
    pub fn quick() -> Self {
        Self {
            depth: IndexDepth::Headers,
            cache: None,
            limit: None,
        }
    }
}

/// One session, as the picker sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    /// The session id — the main transcript's file stem, which equals its
    /// `sessionId` on 808 of 808 files.
    pub session: SessionId,
    /// The main transcript.
    pub transcript: PathBuf,
    /// The `<session-id>/` sidecar directory, when it exists. This is where the
    /// subagent transcripts live, and 79% of transcript files are subagent ones.
    pub sidecar_dir: Option<PathBuf>,
    /// The `<munged-cwd>` directory the session lives under.
    pub project_dir: PathBuf,
    /// The repository the session ran in, read from a record's `cwd`.
    ///
    /// Read rather than derived: the munged directory name is lossy and can be
    /// overridden, so it is not always invertible (ADR-0033).
    pub repo: Option<PathBuf>,
    /// Whether that repository is still on disk. A session whose repository is
    /// gone cannot be replayed over a city.
    pub repo_exists: bool,
    /// First timestamp in the file. Display only (ADR-0014) — but it is what an
    /// operator recognises a session by.
    pub started: Option<WallTime>,
    /// Last timestamp in the file.
    pub ended: Option<WallTime>,
    /// Lines in the main transcript.
    pub records: u64,
    /// `tool_use` blocks in the main transcript.
    pub tool_calls: u32,
    /// Distinct `file_path` values in the main transcript.
    pub files_touched: u32,
    /// `agent-*.jsonl` files under the sidecar directory.
    pub subagents: u32,
    /// `ai-title`, `custom-title`, or the session slug.
    pub title: Option<String>,
    /// The branch the last record reported. `"HEAD"` is dropped, because a
    /// detached head is not a branch (ADR-0018).
    pub branch: Option<String>,
    /// The Claude Code release that wrote the file. The drift signal.
    pub version: Option<String>,
    /// Size of the main transcript.
    pub bytes: u64,
    /// True when the file was too large to scan, so the counts are missing
    /// rather than zero.
    pub counts_partial: bool,
    /// Modification time, in Unix milliseconds — the cache key and the sort
    /// fallback for a session with no timestamps.
    pub modified_ms: i64,
}

impl SessionSummary {
    /// How long the session ran, when both ends are known.
    pub fn duration(&self) -> Option<Duration> {
        self.ended?.duration_since(self.started?)
    }

    /// Whether this session can be replayed over a city right now.
    ///
    /// Keyed on file size rather than on [`SessionSummary::records`], because
    /// [`IndexDepth::Headers`] deliberately leaves the record count unread and
    /// holding a number the caller chose not to gather against a session would
    /// make the fast scan report every session as unreplayable.
    pub fn is_replayable(&self) -> bool {
        self.repo_exists && self.bytes > 0
    }

    /// Reads the whole session — main transcript plus every subagent file — into
    /// a schedule.
    pub fn schedule(&self, mapper: &PathMapper) -> io::Result<ReplaySchedule> {
        ReplaySchedule::from_session_dir(&self.session_dir(), mapper)
    }

    /// The `<munged-cwd>/<session-id>` path `polis-ingest` keys a session on.
    /// It need not exist on disk.
    pub fn session_dir(&self) -> PathBuf {
        self.transcript.with_extension("")
    }

    /// A one-line label for a picker: title, or repository plus date.
    pub fn label(&self) -> String {
        if let Some(title) = &self.title {
            return title.clone();
        }
        let repo = self
            .repo
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown repo");
        format!("{repo} · {}", self.session)
    }
}

/// Every session on this machine, most recent first.
#[derive(Debug, Clone)]
pub struct SessionIndex {
    /// The sessions, sorted most-recent first.
    pub sessions: Vec<SessionSummary>,
    /// Where they were found.
    pub projects_dir: PathBuf,
    /// How many transcripts were read from disk this run.
    pub scanned: usize,
    /// How many came from the cache untouched.
    pub from_cache: usize,
    /// How long the scan took.
    pub elapsed: Duration,
    /// Files that could not be read, with the reason. Never fatal: one
    /// unreadable transcript must not cost the operator the other nine hundred.
    pub errors: Vec<String>,
}

impl SessionIndex {
    /// Scans `~/.claude/projects` with the default options.
    pub fn scan(projects_dir: &Path) -> io::Result<Self> {
        Self::scan_with(projects_dir, &IndexOptions::default())
    }

    /// Scans with explicit options.
    pub fn scan_with(projects_dir: &Path, options: &IndexOptions) -> io::Result<Self> {
        let started = Instant::now();
        let mut cache = options
            .cache
            .as_deref()
            .map(Cache::load)
            .unwrap_or_default();
        let mut errors = Vec::new();
        let mut scanned = 0;
        let mut from_cache = 0;
        let mut sessions = Vec::new();

        let project_dirs = discover_project_dirs(projects_dir)?;
        for project_dir in project_dirs {
            let dirs = match sessions_in(&project_dir) {
                Ok(d) => d,
                Err(e) => {
                    errors.push(format!("{}: {e}", project_dir.display()));
                    continue;
                }
            };
            for session_dir in dirs {
                let transcript = session_dir.with_extension("jsonl");
                let Ok(meta) = fs::metadata(&transcript) else {
                    continue;
                };
                let bytes = meta.len();
                let modified_ms = meta
                    .modified()
                    .map_or(WallTime::UNIX_EPOCH, WallTime::from_system_time)
                    .unix_millis();

                let key = CacheKey {
                    bytes,
                    modified_ms,
                    depth_full: options.depth == IndexDepth::Full,
                };
                if let Some(hit) = cache.get(&transcript, key) {
                    from_cache += 1;
                    sessions.push(hit);
                    continue;
                }
                match summarise(&transcript, &project_dir, &meta, options.depth) {
                    Ok(summary) => {
                        scanned += 1;
                        cache.put(&transcript, key, &summary);
                        sessions.push(summary);
                    }
                    Err(e) => errors.push(format!("{}: {e}", transcript.display())),
                }
            }
        }

        // Most recent first: by end time, falling back to file mtime for a
        // session that never wrote a timestamp.
        sessions.sort_by(|a, b| {
            let ka = a.ended.map_or(a.modified_ms, WallTime::unix_millis);
            let kb = b.ended.map_or(b.modified_ms, WallTime::unix_millis);
            kb.cmp(&ka).then_with(|| a.session.cmp(&b.session))
        });
        if let Some(limit) = options.limit {
            sessions.truncate(limit);
        }
        if let Some(path) = options.cache.as_deref() {
            if let Err(e) = cache.save(path) {
                errors.push(format!("cache not written: {e}"));
            }
        }

        Ok(Self {
            sessions,
            projects_dir: projects_dir.to_path_buf(),
            scanned,
            from_cache,
            elapsed: started.elapsed(),
            errors,
        })
    }

    /// The newest session.
    pub fn most_recent(&self) -> Option<&SessionSummary> {
        self.sessions.first()
    }

    /// Sessions that can be replayed right now, newest first.
    pub fn replayable(&self) -> impl Iterator<Item = &SessionSummary> + '_ {
        self.sessions.iter().filter(|s| s.is_replayable())
    }

    /// Sessions that ran in one repository.
    pub fn for_repo<'a>(&'a self, repo: &'a Path) -> impl Iterator<Item = &'a SessionSummary> + 'a {
        self.sessions
            .iter()
            .filter(move |s| s.repo.as_deref() == Some(repo))
    }

    /// One session by id.
    pub fn get(&self, session: &SessionId) -> Option<&SessionSummary> {
        self.sessions.iter().find(|s| &s.session == session)
    }

    /// How many sessions were found.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// True when the operator has no sessions at all.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Every repository that has sessions, with how many, most sessions first.
    pub fn repositories(&self) -> Vec<(PathBuf, usize)> {
        let mut counts: BTreeMap<PathBuf, usize> = BTreeMap::new();
        for s in &self.sessions {
            if let Some(repo) = &s.repo {
                *counts.entry(repo.clone()).or_insert(0) += 1;
            }
        }
        let mut out: Vec<(PathBuf, usize)> = counts.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }
}

// ---------------------------------------------------------------------------
// Reading one transcript
// ---------------------------------------------------------------------------

/// Builds one summary from one main transcript.
fn summarise(
    transcript: &Path,
    project_dir: &Path,
    meta: &fs::Metadata,
    depth: IndexDepth,
) -> io::Result<SessionSummary> {
    let bytes = meta.len();
    let modified_ms = meta
        .modified()
        .map_or(WallTime::UNIX_EPOCH, WallTime::from_system_time)
        .unix_millis();
    let session = SessionId::new(
        transcript
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default(),
    );
    let session_dir = transcript.with_extension("");

    let head = read_head(transcript, HEAD_BYTES)?;
    let tail = read_tail(transcript, TAIL_BYTES)?;

    let repo = field(&head, "cwd")
        .or_else(|| field(&tail, "cwd"))
        .map(PathBuf::from);
    let repo_exists = repo.as_deref().is_some_and(Path::is_dir);
    let started = field(&head, "timestamp")
        .as_deref()
        .and_then(parse_timestamp);
    let ended = last_field(&tail, "timestamp")
        .as_deref()
        .and_then(parse_timestamp)
        .or(started);
    let version = last_field(&tail, "version").or_else(|| field(&head, "version"));
    let branch = last_field(&tail, "gitBranch")
        .or_else(|| field(&head, "gitBranch"))
        .filter(|b| b != "HEAD" && !b.is_empty());
    let title = last_field(&tail, "customTitle")
        .or_else(|| last_field(&tail, "aiTitle"))
        .or_else(|| field(&head, "slug"));

    let (records, tool_calls, files_touched, counts_partial) = match depth {
        IndexDepth::Headers => (0, 0, 0, true),
        IndexDepth::Full if bytes > MAX_FULL_READ => (0, 0, 0, true),
        IndexDepth::Full => {
            let body = fs::read(transcript)?;
            let counts = count(&body);
            (counts.0, counts.1, counts.2, false)
        }
    };

    let sidecar_dir = session_dir.is_dir().then(|| session_dir.clone());
    let subagents = sidecar_dir
        .as_deref()
        .map_or(0, |d| count_agent_files(&d.join("subagents"), 0));

    Ok(SessionSummary {
        session,
        transcript: transcript.to_path_buf(),
        sidecar_dir,
        project_dir: project_dir.to_path_buf(),
        repo,
        repo_exists,
        started,
        ended,
        records,
        tool_calls,
        files_touched,
        subagents,
        title,
        branch,
        version,
        bytes,
        counts_partial,
        modified_ms,
    })
}

/// Counts records, tool calls and distinct touched files by byte scan.
///
/// The on-disk format is compact JSON with no space after a colon — verified
/// over the whole corpus — so `"type":"tool_use"` is a literal byte sequence.
/// Parsing 300 MB of JSON to learn two numbers would cost tens of seconds; this
/// costs a memory-bandwidth pass.
#[allow(clippy::naive_bytecount)] // the `bytecount` crate is not a workspace dependency
fn count(body: &[u8]) -> (u64, u32, u32) {
    let records = u64::try_from(body.iter().filter(|b| **b == b'\n').count()).unwrap_or(u64::MAX);
    // Claude Code writes compact JSON, but a re-serialized or hand-written
    // fixture may not, so both spellings are counted.
    let tool_calls = u32::try_from(
        count_needle(body, br#""type":"tool_use""#) + count_needle(body, br#""type": "tool_use""#),
    )
    .unwrap_or(u32::MAX);
    let mut paths: BTreeSet<Vec<u8>> = BTreeSet::new();
    for needle in [&br#""file_path":""#[..], &br#""file_path": ""#[..]] {
        let mut from = 0;
        while let Some(hit) = find(&body[from..], needle) {
            let start = from + hit + needle.len();
            if let Some(end) = json_string_end(&body[start..]) {
                let mut key = body[start..start + end].to_vec();
                key.make_ascii_lowercase();
                // Both separators occur on Windows — 11 606 backslash to 1 268
                // forward — so the same file must not count twice. A backslash
                // arrives JSON-escaped as two bytes, so the run is collapsed
                // rather than replaced one-for-one.
                for b in &mut key {
                    if *b == b'\\' {
                        *b = b'/';
                    }
                }
                key.dedup_by(|a, b| *a == b'/' && *b == b'/');
                paths.insert(key);
                from = start + end;
            } else {
                from = start;
            }
            if from >= body.len() {
                break;
            }
        }
    }
    (
        records,
        tool_calls,
        u32::try_from(paths.len()).unwrap_or(u32::MAX),
    )
}

/// The offset of the closing quote of a JSON string body, honouring escapes.
fn json_string_end(bytes: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// The first value of a top-level-ish string field, by byte search.
fn field(bytes: &[u8], key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let hit = find(bytes, needle.as_bytes())?;
    let start = hit + needle.len();
    let end = json_string_end(&bytes[start..])?;
    unescape(&bytes[start..start + end])
}

/// The last value of a string field, by byte search.
fn last_field(bytes: &[u8], key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let hit = rfind(bytes, needle.as_bytes())?;
    let start = hit + needle.len();
    let end = json_string_end(&bytes[start..])?;
    unescape(&bytes[start..start + end])
}

/// Turns a JSON string body into a `String`, dropping it if it is not UTF-8.
fn unescape(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    if !text.contains('\\') {
        return Some(text.to_owned());
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('u') => {
                // Skip the four hex digits rather than decoding: nothing this
                // module reads out of a transcript is outside ASCII in practice,
                // and a wrong character in a title is cheaper than a panic.
                for _ in 0..4 {
                    let _ = chars.next();
                }
                out.push('\u{fffd}');
            }
            Some(other) => out.push(other),
            None => break,
        }
    }
    Some(out)
}

/// First occurrence of `needle` in `haystack`.
///
/// Scans for one anchor byte and only then compares the whole needle.
/// `slice::position` over bytes vectorises; the whole-window comparison
/// `slice::windows(n).position(..)` would do does not, and over 300 MB of
/// transcript that difference is ten seconds.
///
/// The anchor is chosen to be *rare in a transcript*, not simply the first byte:
/// `"` is roughly a tenth of the bytes in JSON and `:` is not far behind, so
/// anchoring on either would verify an order of magnitude more often than it
/// needs to. An underscore is rare in both JSON punctuation and English prose,
/// and every needle this module uses has one.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let anchor_at = anchor_index(needle);
    let anchor = needle[anchor_at];
    let mut from = anchor_at;
    while from < haystack.len() {
        let offset = haystack[from..].iter().position(|b| *b == anchor)?;
        let at = from + offset;
        let start = at - anchor_at;
        if start + needle.len() <= haystack.len()
            && &haystack[start..start + needle.len()] == needle
        {
            return Some(start);
        }
        from = at + 1;
    }
    None
}

/// Which byte of a needle to scan for.
fn anchor_index(needle: &[u8]) -> usize {
    needle
        .iter()
        .rposition(|b| *b == b'_')
        .or_else(|| needle.iter().rposition(u8::is_ascii_alphanumeric))
        .unwrap_or(needle.len() - 1)
}

/// Last occurrence of `needle` in `haystack`.
fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

/// How many times `needle` occurs, non-overlapping.
fn count_needle(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    let mut n = 0;
    let mut from = 0;
    while let Some(hit) = find(&haystack[from..], needle) {
        n += 1;
        from += hit + needle.len();
        if from >= haystack.len() {
            break;
        }
    }
    n
}

/// Reads at most `limit` bytes from the front of a file.
fn read_head(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let mut buf = Vec::new();
    file.take(limit).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Reads at most `limit` bytes from the end of a file, starting at a line
/// boundary so a truncated first line cannot be mistaken for a record.
fn read_tail(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    let from = len.saturating_sub(limit);
    file.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    if from > 0 {
        if let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            buf.drain(..=nl);
        }
    }
    Ok(buf)
}

/// Counts `agent-*.jsonl` under a directory, at any depth.
///
/// Depth-limited and glob-based: a workflow agent sits two directories deeper
/// than a directly spawned one, and assuming a fixed depth orphans four out of
/// five subagents (ADR-0013).
#[allow(clippy::case_sensitive_file_extension_comparisons)]
// `agent-<id>.jsonl` is written by Claude Code in exactly that case, and a
// case-insensitive match here would also accept an unrelated `AGENT-x.JSONL`.
fn count_agent_files(dir: &Path, depth: usize) -> u32 {
    if depth > 4 {
        return 0;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut n = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            n += count_agent_files(&path, depth + 1);
        } else if path
            .file_name()
            .and_then(|f| f.to_str())
            .is_some_and(|f| f.starts_with("agent-") && f.ends_with(".jsonl"))
        {
            n += 1;
        }
    }
    n
}

// ---------------------------------------------------------------------------
// The cache
// ---------------------------------------------------------------------------

/// What makes a cached entry still valid.
///
/// Transcripts are append-only, so `(size, mtime)` is a sound key: a file that
/// has not changed cannot have different contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct CacheKey {
    bytes: u64,
    modified_ms: i64,
    /// A `Headers`-depth entry has no counts, so it must not satisfy a `Full`
    /// request.
    depth_full: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    key: CacheKey,
    summary: SessionSummary,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    version: u32,
    entries: BTreeMap<String, CacheEntry>,
    #[serde(skip)]
    dirty: bool,
}

impl Cache {
    fn load(path: &Path) -> Self {
        let Ok(text) = fs::read_to_string(path) else {
            return Self {
                version: CACHE_VERSION,
                ..Self::default()
            };
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(cache) if cache.version == CACHE_VERSION => cache,
            // A cache from another format version is discarded, not guessed at.
            _ => Self {
                version: CACHE_VERSION,
                ..Self::default()
            },
        }
    }

    fn get(&self, path: &Path, key: CacheKey) -> Option<SessionSummary> {
        let entry = self.entries.get(&path.to_string_lossy().to_string())?;
        if entry.key != key {
            return None;
        }
        let mut summary = entry.summary.clone();
        // The repository may have been deleted since the cache was written, and
        // that is precisely the thing the picker must not get wrong.
        summary.repo_exists = summary.repo.as_deref().is_some_and(Path::is_dir);
        Some(summary)
    }

    fn put(&mut self, path: &Path, key: CacheKey, summary: &SessionSummary) {
        self.entries.insert(
            path.to_string_lossy().to_string(),
            CacheEntry {
                key,
                summary: summary.clone(),
            },
        );
        self.dirty = true;
    }

    fn save(&self, path: &Path) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string(self).map_err(io::Error::other)?;
        fs::write(path, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_are_found_by_byte_search_in_compact_json() {
        let line = br#"{"type":"user","cwd":"C:\\coding\\agentolis","timestamp":"2026-08-24T22:43:22.764Z","version":"2.1.229"}"#;
        assert_eq!(field(line, "cwd").as_deref(), Some(r"C:\coding\agentolis"));
        assert_eq!(
            field(line, "timestamp").as_deref(),
            Some("2026-08-24T22:43:22.764Z")
        );
        assert_eq!(field(line, "version").as_deref(), Some("2.1.229"));
        assert!(field(line, "nope").is_none());
    }

    #[test]
    fn the_last_timestamp_wins_for_the_end_time() {
        let body = br#"{"timestamp":"2026-08-24T10:00:00.000Z"}
{"timestamp":"2026-08-24T12:30:00.000Z"}"#;
        assert_eq!(
            last_field(body, "timestamp").as_deref(),
            Some("2026-08-24T12:30:00.000Z")
        );
    }

    #[test]
    fn counting_is_a_byte_scan_and_normalises_both_separators() {
        let body = br#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"C:\\repo\\src\\a.rs"}}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"C:/repo/src/a.rs"}}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"C:/repo/src/b.rs"}}]}}
"#;
        let (records, tools, files) = count(body);
        assert_eq!(records, 3);
        assert_eq!(tools, 3);
        assert_eq!(
            files, 2,
            "the same file spelled two ways must not count twice"
        );
    }

    #[test]
    fn the_anchored_search_finds_what_a_naive_one_would() {
        let hay = br#"{"a":"_x_","file_path":"src/a.rs","b":"file_path"}"#;
        let needle = br#""file_path":""#;
        let naive = hay
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("naive finds it");
        assert_eq!(find(hay, needle), Some(naive));
        assert_eq!(rfind(hay, needle), Some(naive));
        // The anchor is the underscore, not the leading quote.
        assert_eq!(anchor_index(needle), 5);
        // Degenerate inputs.
        assert_eq!(find(b"", needle), None);
        assert_eq!(find(b"short", needle), None);
        assert_eq!(find(hay, b""), None);
        // A needle with no underscore or alphanumeric still works.
        assert_eq!(find(br#"a:"b"#, br#":""#), Some(1));
        // Every occurrence is counted, not just the first.
        assert_eq!(count_needle(b"ababab", b"ab"), 3);
        assert_eq!(count_needle(b"aaaa", b"aa"), 2, "non-overlapping");
    }

    #[test]
    fn an_escaped_quote_inside_a_path_does_not_end_the_string() {
        let line = br#"{"file_path":"C:\\weird\"name.rs","x":1}"#;
        let (_, _, files) = count(line);
        assert_eq!(files, 1);
        assert_eq!(json_string_end(br#"a\"b","#), Some(4));
    }

    #[test]
    fn a_summary_reports_what_it_could_not_learn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path().join("C--nowhere");
        fs::create_dir_all(&project).unwrap();
        let transcript = project.join("11111111-2222-3333-4444-555555555555.jsonl");
        fs::write(
            &transcript,
            b"{\"type\":\"user\",\"cwd\":\"C:/definitely/not/here\",\"timestamp\":\"2026-08-24T10:00:00.000Z\",\"sessionId\":\"11111111-2222-3333-4444-555555555555\"}\n",
        )
        .unwrap();

        let index = SessionIndex::scan_with(
            dir.path(),
            &IndexOptions {
                depth: IndexDepth::Full,
                cache: None,
                limit: None,
            },
        )
        .expect("scan");
        assert_eq!(index.len(), 1);
        let s = &index.sessions[0];
        assert_eq!(s.repo.as_deref(), Some(Path::new("C:/definitely/not/here")));
        assert!(
            !s.repo_exists,
            "a session whose repository is gone must say so"
        );
        assert!(!s.is_replayable());
        assert_eq!(s.records, 1);
        assert!(!s.counts_partial);
        assert!(s.started.is_some() && s.ended.is_some());
    }

    #[test]
    fn the_cache_is_keyed_on_size_and_mtime_and_re_checks_the_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path().join("C--somewhere");
        fs::create_dir_all(&project).unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let transcript = project.join("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl");
        let line = format!(
            "{{\"type\":\"user\",\"cwd\":\"{}\",\"timestamp\":\"2026-08-24T10:00:00.000Z\"}}\n",
            repo.display().to_string().replace('\\', "/")
        );
        fs::write(&transcript, line).unwrap();
        let cache_path = dir.path().join("cache.json");
        let options = IndexOptions {
            depth: IndexDepth::Full,
            cache: Some(cache_path.clone()),
            limit: None,
        };

        let first = SessionIndex::scan_with(dir.path(), &options).expect("first scan");
        assert_eq!(first.scanned, 1);
        assert_eq!(first.from_cache, 0);
        assert!(first.sessions[0].repo_exists);

        let second = SessionIndex::scan_with(dir.path(), &options).expect("second scan");
        assert_eq!(second.scanned, 0, "an unchanged transcript is not re-read");
        assert_eq!(second.from_cache, 1);

        // The repository disappears; the cached answer must not survive it.
        fs::remove_dir_all(&repo).unwrap();
        let third = SessionIndex::scan_with(dir.path(), &options).expect("third scan");
        assert_eq!(third.from_cache, 1);
        assert!(!third.sessions[0].repo_exists);
    }

    #[test]
    fn sessions_sort_most_recent_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path().join("C--x");
        fs::create_dir_all(&project).unwrap();
        for (id, ts) in [
            ("00000000-0000-0000-0000-000000000001", "2026-08-01"),
            ("00000000-0000-0000-0000-000000000002", "2026-09-01"),
            ("00000000-0000-0000-0000-000000000003", "2026-07-01"),
        ] {
            fs::write(
                project.join(format!("{id}.jsonl")),
                format!("{{\"type\":\"user\",\"timestamp\":\"{ts}T10:00:00.000Z\"}}\n"),
            )
            .unwrap();
        }
        let index = SessionIndex::scan_with(dir.path(), &IndexOptions::quick()).expect("scan");
        let order: Vec<&str> = index.sessions.iter().map(|s| s.session.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "00000000-0000-0000-0000-000000000002",
                "00000000-0000-0000-0000-000000000001",
                "00000000-0000-0000-0000-000000000003",
            ]
        );
        // Headers depth leaves the counts unread and says so.
        assert!(index.sessions[0].counts_partial);
    }
}
