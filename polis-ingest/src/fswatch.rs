//! Channel C — the filesystem watcher (PRD §4.3).
//!
//! > Completely out of band; zero agent impact. Gives you the ground truth of
//! > what actually changed on disk, which OTel does not.
//!
//! # Attribution is advisory, and the type system says so
//!
//! The filesystem does not know which agent wrote. PRD §4.3 asks for write
//! events to be correlated against the tool-call stream within a ±2 s window
//! keyed on path, and [`CorrelationWindow`] is that window — anchored on the
//! **hook or OTel clock**, which is stamped at the moment of the call, never on
//! a transcript timestamp: 20% of transcript files contain a backwards step and
//! one observed jump was 60 seconds (ADR-0014).
//!
//! What PRD §4.3 also says is that where attribution must be certain, Polis can
//! *"fall back to the `FileChanged` hook"*. It cannot.
//! `docs/verified/hooks-schema.md` §9.2 establishes that the hook watches a
//! literal, explicitly named filename list and that its payload carries no
//! `tool_name` and no `tool_use_id` — it is exactly as attribution-blind as this
//! channel while additionally costing a process spawn. `PreToolUse` claims are
//! the only authoritative channel (ADR-0003), and PRD §17 is explicit that
//! *anything that drives an alert must come from an authoritative channel*.
//!
//! So this module makes the advisory nature structural rather than documentary:
//!
//! * **Nothing this module puts on the bus carries an attribution.**
//!   [`polis_events::FsEvent`] has no thread, worker or session field, and the
//!   envelope this module stamps leaves every identity field `None`. There is
//!   no place in a bus event where a guess could hide.
//! * **The correlation result is a separate type with no authoritative form.**
//!   [`AdvisoryAttribution`] has no public fields, no `Deref`, and no
//!   conversion into a bare `ThreadId`. Every accessor is spelled
//!   `advisory_*`, and [`AdvisoryAttribution::is_authoritative`] is a `const fn`
//!   that returns `false` — there is no constructor anywhere that yields
//!   anything else.
//!
//! A caller therefore cannot pass an FS-derived attribution to something
//! expecting a certain one without writing the word `advisory` at the call site.
//!
//! # Ignores
//!
//! `.git/`, `node_modules/`, `target/`, `dist/`, and anything in `.gitignore`.
//! The `.gitignore` half is [`IgnoreSet`], which parses the format properly —
//! nested ignore files, negation, anchoring, directory-only rules and `**`.
//! Substring matching would be both wrong (`!keep.log` after `*.log`) and
//! useless at scale: on a repository with a large generated tree the difference
//! between a parsed ignore set and a substring test is the difference between a
//! working watcher and a firehose.
//!
//! Note that this governs the **watch**; PRD §8's industrial zone still
//! *renders* `node_modules` as a single dull mass, and that classification lives
//! in `polis-repo`, not here.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use polis_events::{
    Channel, ControlEvent, FsEvent, LogicalPath, PathMapper, ThreadId, ToolKind, WorkerId,
    WorktreeId,
};

use crate::bus::EventSink;
use crate::normalize::{self, FsChange};
use crate::{IngestSource, SourceHealth};

/// Directory names excluded from the watch regardless of any `.gitignore`
/// (PRD §4.3).
///
/// Matched as a whole path component at any depth and case-insensitively —
/// `core.ignorecase` is on by default on Windows and macOS, and PRD §16 diffs
/// golden files across two operating systems, so the watch set must not depend
/// on which one is running.
pub const HARD_EXCLUDED_DIRS: &[&str] = &[".git", "node_modules", "target", "dist"];

/// PRD §4.3's correlation window: ±2 s, keyed on logical path.
pub const CORRELATION_WINDOW: Duration = Duration::from_secs(2);

/// How long a path must be quiet before its change is emitted.
///
/// An editor's save is three to five `notify` events — a temp file, a rename, a
/// truncate, a write — and a compiler writing a target tree is thousands. 200 ms
/// is below the threshold at which a human reads the city as laggy and above the
/// duration of every save sequence observed.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(200);

/// The longest pattern [`IgnoreSet`] will compile, in path segments.
const MAX_PATTERN_SEGMENTS: usize = 64;

/// The layout key a watched path reduces to (PRD §7.6).
///
/// Worktree and logical path stay a **pair** rather than one composite: the
/// worktree is a separate dimension layered over one shared base map, never
/// folded into the key the city is drawn from.
pub type Key = (WorktreeId, LogicalPath);

/// Locks without ever panicking on a poisoned mutex.
///
/// A panic in one watcher thread must not take the ingest stack down with it:
/// PRD §4 makes every channel independently optional, and a poisoned lock is a
/// worse reason to lose Channel C than any real failure.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Tuning for [`FsWatcher`], all of it defaulted.
#[derive(Debug, Clone, Copy)]
pub struct FsWatchConfig {
    /// Quiet period before a path's coalesced change is emitted.
    pub debounce: Duration,
    /// Depth of the queue between `notify`'s thread and the debouncer.
    ///
    /// Overflowing it means events were lost, which is indistinguishable from
    /// the OS queue overflowing — so it sets the same re-scan flag.
    pub raw_capacity: usize,
    /// Ceiling on the directory walk that discovers nested `.gitignore` files.
    ///
    /// A pathological tree must not turn startup into a full crawl of the disk.
    pub max_walk_entries: usize,
    /// PRD §4.3's correlation half-width. [`CORRELATION_WINDOW`] outside tests.
    pub correlation_window: Duration,
}

impl Default for FsWatchConfig {
    fn default() -> Self {
        Self {
            debounce: DEFAULT_DEBOUNCE,
            raw_capacity: 8_192,
            max_walk_entries: 200_000,
            correlation_window: CORRELATION_WINDOW,
        }
    }
}

// ---------------------------------------------------------------------------
// Attribution — advisory by construction
// ---------------------------------------------------------------------------

/// One authoritative observation of an agent touching a path.
///
/// Fed in by the world thread from a `PreToolUse` hook or an OTel tool span —
/// the two channels that actually know which agent is acting.
#[derive(Debug, Clone)]
pub struct ToolTouch {
    /// The thread that made the call.
    pub thread: ThreadId,
    /// The worker, when the call came from inside a subagent.
    pub worker: Option<WorkerId>,
    /// Which tool.
    pub tool: ToolKind,
    /// The logical path it named.
    pub path: Key,
    /// **The hook or OTel clock**, i.e. `EventMeta::observed` — the moment Polis
    /// received the call. Never a transcript timestamp (ADR-0014).
    pub at: Instant,
}

/// An attribution inferred from timing. **Never authoritative** (ADR-0003,
/// PRD §17).
///
/// There is no non-advisory counterpart of this type and no way to unwrap it
/// into one: the fields are private, there is no `Deref`, no `From`, and every
/// accessor is spelled `advisory_*` so that a reviewer reading a call site sees
/// the word. [`Self::is_authoritative`] is a `const fn` returning `false`, and
/// no constructor produces anything else.
///
/// Use it to *suggest* — to tint a building, to order a drill-down list. Never
/// to raise a contention alert: PRD §11.3's claims come from `PreToolUse`.
#[derive(Debug, Clone)]
pub struct AdvisoryAttribution {
    thread: ThreadId,
    worker: Option<WorkerId>,
    tool: ToolKind,
    lag: Duration,
    candidate_threads: usize,
}

impl AdvisoryAttribution {
    /// Always `false`, on every value of this type, by construction.
    pub const fn is_authoritative(&self) -> bool {
        false
    }

    /// The thread the timing correlation *suggests*.
    pub fn advisory_thread(&self) -> &ThreadId {
        &self.thread
    }

    /// The worker the correlation suggests, when the matching call came from
    /// inside a subagent.
    pub fn advisory_worker(&self) -> Option<&WorkerId> {
        self.worker.as_ref()
    }

    /// The tool the correlation suggests.
    pub fn advisory_tool(&self) -> &ToolKind {
        &self.tool
    }

    /// How far apart the write and the tool call were. Smaller is better
    /// evidence; nothing here is proof.
    pub fn lag(&self) -> Duration {
        self.lag
    }

    /// How many distinct threads had a call on this path inside the window.
    pub fn candidate_threads(&self) -> usize {
        self.candidate_threads
    }

    /// True when more than one thread could have written. Even a `false` here
    /// does not make the attribution authoritative.
    pub fn is_ambiguous(&self) -> bool {
        self.candidate_threads > 1
    }
}

/// PRD §4.3's ±2 s correlation window, keyed on logical path.
///
/// Kept as a standalone type so it can be exercised without a filesystem: the
/// interesting behaviour is entirely about clocks and ambiguity.
#[derive(Debug)]
pub struct CorrelationWindow {
    window: Duration,
    touches: VecDeque<ToolTouch>,
    capacity: usize,
}

impl CorrelationWindow {
    /// A window of [`CORRELATION_WINDOW`].
    pub fn new() -> Self {
        Self::with_window(CORRELATION_WINDOW)
    }

    /// A window of a given half-width.
    pub fn with_window(window: Duration) -> Self {
        Self {
            window,
            touches: VecDeque::new(),
            capacity: 4_096,
        }
    }

    /// Records an authoritative tool call and evicts anything now outside the
    /// window.
    pub fn observe(&mut self, touch: ToolTouch) {
        let now = touch.at;
        self.touches.push_back(touch);
        while self.touches.front().is_some_and(|t| {
            now.checked_duration_since(t.at)
                .is_some_and(|d| d > self.window)
        }) {
            self.touches.pop_front();
        }
        while self.touches.len() > self.capacity {
            self.touches.pop_front();
        }
    }

    /// Correlates a filesystem write against the recorded calls.
    ///
    /// Returns the closest call on the same logical path within ±`window`, or
    /// `None`. The result is [`AdvisoryAttribution`] and cannot be anything
    /// else.
    pub fn correlate(&self, path: &Key, at: Instant) -> Option<AdvisoryAttribution> {
        let mut best: Option<(&ToolTouch, Duration)> = None;
        let mut threads: Vec<&ThreadId> = Vec::new();
        for touch in &self.touches {
            if &touch.path != path {
                continue;
            }
            let Some(lag) = gap(at, touch.at) else {
                continue;
            };
            if lag > self.window {
                continue;
            }
            if !threads.contains(&&touch.thread) {
                threads.push(&touch.thread);
            }
            if best.is_none_or(|(_, best_lag)| lag < best_lag) {
                best = Some((touch, lag));
            }
        }
        let (touch, lag) = best?;
        Some(AdvisoryAttribution {
            thread: touch.thread.clone(),
            worker: touch.worker.clone(),
            tool: touch.tool.clone(),
            lag,
            candidate_threads: threads.len(),
        })
    }

    /// How many calls are currently in the window.
    pub fn len(&self) -> usize {
        self.touches.len()
    }

    /// True when nothing is in the window.
    pub fn is_empty(&self) -> bool {
        self.touches.is_empty()
    }
}

impl Default for CorrelationWindow {
    fn default() -> Self {
        Self::new()
    }
}

/// Absolute distance between two monotonic readings, `None` if either is
/// somehow unrepresentable.
fn gap(a: Instant, b: Instant) -> Option<Duration> {
    a.checked_duration_since(b)
        .or_else(|| b.checked_duration_since(a))
}

// ---------------------------------------------------------------------------
// .gitignore
// ---------------------------------------------------------------------------

/// A parsed set of `.gitignore` files, nested ignore files and negations
/// included (PRD §4.3).
///
/// Keyed by the ignore file's directory relative to the watch root, because
/// that is what gitignore semantics are defined against: a pattern with a slash
/// in it anchors to the directory of the file it appears in, a pattern without
/// one matches a basename at any depth below that directory, and a deeper file
/// overrides a shallower one.
///
/// Matching folds ASCII case, consistent with [`LogicalPath`] (ADR-0028) and
/// with `core.ignorecase`, so the watch set is the same on all three platforms.
#[derive(Debug, Clone, Default)]
pub struct IgnoreSet {
    /// Ignore-file directory, slash-joined and relative to the root (`""` for
    /// the root itself) -> that file's rules, in file order.
    ///
    /// `BTreeMap`, not `HashMap`: this is walked to decide what the watcher
    /// sees, and PRD §7.4 does not permit iteration order to reach anything the
    /// layout can observe.
    files: BTreeMap<String, Vec<Rule>>,
}

impl IgnoreSet {
    /// An empty set. [`HARD_EXCLUDED_DIRS`] applies regardless of this.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads every `.gitignore` under `root`, pruning the walk with the rules
    /// it has already read.
    ///
    /// Never fails: an unreadable directory or a non-UTF-8 ignore file is
    /// skipped. A repository whose ignores cannot be read is a noisier watch,
    /// not a dead channel (ADR-0011).
    pub fn load(root: &Path, max_entries: usize) -> Self {
        let mut set = Self::new();
        let mut stack = vec![(root.to_path_buf(), String::new())];
        let mut visited = 0usize;
        while let Some((dir, base)) = stack.pop() {
            visited += 1;
            if visited > max_entries {
                break;
            }
            if let Ok(text) = fs::read_to_string(dir.join(".gitignore")) {
                set.add_file(&base, &text);
            }
            set.push_children(&dir, &base, &mut stack);
        }
        set
    }

    /// Queues the sub-directories of `dir` that are not already excluded.
    fn push_children(&self, dir: &Path, base: &str, stack: &mut Vec<(PathBuf, String)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut children: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.path())
            .collect();
        // PRD §16 diffs golden files across two operating systems and `read_dir`
        // order differs between NTFS, ext4 and APFS.
        children.sort();
        for child in children {
            let Some(name) = child.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if is_hard_excluded_name(name) {
                continue;
            }
            let rel = if base.is_empty() {
                name.to_owned()
            } else {
                format!("{base}/{name}")
            };
            if self.is_ignored(&rel, true) {
                continue;
            }
            stack.push((child, rel));
        }
    }

    /// Adds the rules of one ignore file, whose directory is `base` relative to
    /// the root.
    pub fn add_file(&mut self, base: &str, contents: &str) {
        let rules: Vec<Rule> = contents.lines().filter_map(parse_rule).collect();
        if rules.is_empty() {
            return;
        }
        self.files.entry(base.to_owned()).or_default().extend(rules);
    }

    /// True when git would ignore `rel`, a slash-separated path relative to the
    /// root.
    ///
    /// Walks the path from the root down, one component at a time, exactly as
    /// git does. The moment an ancestor directory is excluded the answer is
    /// `true` and the walk stops — *it is not possible to re-include a file if a
    /// parent directory of that file is excluded*, and a matcher that tested
    /// only the full path would get that backwards.
    pub fn is_ignored(&self, rel: &str, is_dir: bool) -> bool {
        let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
        for i in 0..comps.len() {
            let here_is_dir = is_dir || i + 1 < comps.len();
            if self.decide(&comps, i, here_is_dir) == Some(true) {
                return true;
            }
        }
        false
    }

    /// The last matching rule's verdict for `comps[..=i]`, shallowest ignore
    /// file first so that a deeper one wins.
    fn decide(&self, comps: &[&str], i: usize, is_dir: bool) -> Option<bool> {
        let mut decision = None;
        for depth in 0..=i {
            let base = comps[..depth].join("/");
            let Some(rules) = self.files.get(&base) else {
                continue;
            };
            let rel = comps[depth..=i].join("/");
            for rule in rules {
                if rule.matches(&rel, comps[i], is_dir) {
                    decision = Some(!rule.negated);
                }
            }
        }
        decision
    }

    /// How many rules were compiled, across every ignore file.
    pub fn rule_count(&self) -> usize {
        self.files.values().map(Vec::len).sum()
    }
}

/// One `.gitignore` line, compiled.
#[derive(Debug, Clone)]
struct Rule {
    segments: Vec<Segment>,
    /// `!pattern` — re-includes rather than excludes.
    negated: bool,
    /// `pattern/` — matches directories only.
    dir_only: bool,
    /// The pattern contained a slash, so it anchors to the ignore file's
    /// directory instead of matching a basename at any depth.
    anchored: bool,
}

impl Rule {
    fn matches(&self, rel: &str, basename: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        if self.anchored {
            let comps: Vec<&str> = rel.split('/').collect();
            match_segments(&self.segments, &comps)
        } else {
            // Unanchored patterns contain no slash, so they are exactly one
            // segment and match the file name at any depth.
            self.segments.len() == 1 && match_one(&self.segments[0], basename)
        }
    }
}

/// One slash-separated piece of a compiled pattern.
#[derive(Debug, Clone)]
enum Segment {
    /// `**` — zero or more path segments.
    AnyDepth,
    /// No metacharacters; an ASCII-folded equality test.
    Literal(String),
    /// Compiled wildcards.
    Glob(Vec<Tok>),
}

/// One token of a single-segment glob.
#[derive(Debug, Clone)]
enum Tok {
    Char(char),
    /// `?` — one character, never a separator.
    Any,
    /// `*` — any run of characters, never a separator.
    Star,
    Class {
        negated: bool,
        items: Vec<ClassItem>,
    },
}

/// One member of a `[...]` character class.
#[derive(Debug, Clone, Copy)]
enum ClassItem {
    One(char),
    Range(char, char),
}

/// Compiles one `.gitignore` line, or `None` for a blank or comment line.
fn parse_rule(line: &str) -> Option<Rule> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let line = trim_trailing_unescaped_space(line);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (negated, body) = match line.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    // `\#` and `\!` are how a pattern starts with a literal one of those.
    let body = if body.starts_with("\\#") || body.starts_with("\\!") {
        body.strip_prefix('\\').unwrap_or(body)
    } else {
        body
    };
    let (dir_only, body) = match body.strip_suffix('/') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    if body.is_empty() {
        return None;
    }
    // A slash anywhere but the end anchors the pattern to this file's directory.
    let anchored = body.contains('/');
    let body = body.strip_prefix('/').unwrap_or(body);
    let segments: Vec<Segment> = body
        .split('/')
        .filter(|s| !s.is_empty())
        .take(MAX_PATTERN_SEGMENTS)
        .map(compile_segment)
        .collect();
    if segments.is_empty() {
        return None;
    }
    Some(Rule {
        segments,
        negated,
        dir_only,
        anchored,
    })
}

/// Strips trailing whitespace unless it was escaped with a backslash.
fn trim_trailing_unescaped_space(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
        let mut backslashes = 0usize;
        let mut j = end - 1;
        while j > 0 && bytes[j - 1] == b'\\' {
            backslashes += 1;
            j -= 1;
        }
        if backslashes % 2 == 1 {
            break;
        }
        end -= 1;
    }
    // `end` only ever moved back over ASCII space or tab, so it is a character
    // boundary (ADR-0046: never slice on an index derived from other text).
    line.get(..end).unwrap_or(line)
}

/// Compiles one pattern segment.
fn compile_segment(segment: &str) -> Segment {
    if segment == "**" {
        return Segment::AnyDepth;
    }
    if segment
        .bytes()
        .any(|b| matches!(b, b'*' | b'?' | b'[' | b'\\'))
    {
        Segment::Glob(compile_glob(segment))
    } else {
        Segment::Literal(segment.to_ascii_lowercase())
    }
}

/// Compiles a single-segment glob into tokens.
fn compile_glob(pattern: &str) -> Vec<Tok> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                toks.push(Tok::Star);
                i += 1;
            }
            '?' => {
                toks.push(Tok::Any);
                i += 1;
            }
            '\\' if i + 1 < chars.len() => {
                toks.push(Tok::Char(chars[i + 1].to_ascii_lowercase()));
                i += 2;
            }
            // An unterminated class is a literal bracket, which is what git
            // does and what a `[` in a filename needs.
            '[' => {
                if let Some((tok, next)) = compile_class(&chars, i) {
                    toks.push(tok);
                    i = next;
                } else {
                    toks.push(Tok::Char('['));
                    i += 1;
                }
            }
            c => {
                toks.push(Tok::Char(c.to_ascii_lowercase()));
                i += 1;
            }
        }
    }
    toks
}

/// Compiles a `[...]` class starting at `start`, returning the token and the
/// index just past the closing bracket.
fn compile_class(chars: &[char], start: usize) -> Option<(Tok, usize)> {
    let mut i = start + 1;
    let negated = matches!(chars.get(i), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    while i < chars.len() {
        let c = chars[i];
        // A `]` immediately after `[` or `[!` is a literal `]`.
        if c == ']' && !first {
            return Some((Tok::Class { negated, items }, i + 1));
        }
        first = false;
        if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|e| *e != ']') {
            items.push(ClassItem::Range(
                c.to_ascii_lowercase(),
                chars[i + 2].to_ascii_lowercase(),
            ));
            i += 3;
        } else {
            items.push(ClassItem::One(c.to_ascii_lowercase()));
            i += 1;
        }
    }
    None
}

/// Matches a compiled pattern against a component list, `**` included.
fn match_segments(pats: &[Segment], comps: &[&str]) -> bool {
    match pats.split_first() {
        None => comps.is_empty(),
        Some((Segment::AnyDepth, rest)) => {
            (0..=comps.len()).any(|i| match_segments(rest, &comps[i..]))
        }
        Some((seg, rest)) => {
            !comps.is_empty() && match_one(seg, comps[0]) && match_segments(rest, &comps[1..])
        }
    }
}

/// Matches one compiled segment against one path component.
fn match_one(segment: &Segment, comp: &str) -> bool {
    match segment {
        // A bare `**` behaves as `*`, per gitignore's "other consecutive
        // asterisks are considered regular asterisks".
        Segment::AnyDepth => true,
        Segment::Literal(text) => text.eq_ignore_ascii_case(comp),
        Segment::Glob(toks) => {
            let text: Vec<char> = comp.chars().map(|c| c.to_ascii_lowercase()).collect();
            glob_matches(toks, &text)
        }
    }
}

/// Backtracking glob match. Linear in the common case, and bounded because a
/// single `*` backtrack position is remembered rather than recursing.
fn glob_matches(toks: &[Tok], text: &[char]) -> bool {
    let (mut ti, mut xi) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while xi < text.len() {
        let advanced = match toks.get(ti) {
            Some(Tok::Star) => {
                star = Some((ti, xi));
                ti += 1;
                true
            }
            Some(Tok::Any) => {
                ti += 1;
                xi += 1;
                true
            }
            Some(Tok::Char(c)) if *c == text[xi] => {
                ti += 1;
                xi += 1;
                true
            }
            Some(Tok::Class { negated, items }) if class_matches(*negated, items, text[xi]) => {
                ti += 1;
                xi += 1;
                true
            }
            _ => false,
        };
        if advanced {
            continue;
        }
        let Some((star_t, star_x)) = star else {
            return false;
        };
        star = Some((star_t, star_x + 1));
        xi = star_x + 1;
        ti = star_t + 1;
    }
    toks[ti.min(toks.len())..]
        .iter()
        .all(|t| matches!(t, Tok::Star))
}

/// Whether a character satisfies a `[...]` class.
fn class_matches(negated: bool, items: &[ClassItem], c: char) -> bool {
    let hit = items.iter().any(|item| match item {
        ClassItem::One(one) => *one == c,
        ClassItem::Range(lo, hi) => (*lo..=*hi).contains(&c),
    });
    hit != negated
}

/// Whether a path component is one of [`HARD_EXCLUDED_DIRS`].
fn is_hard_excluded_name(name: &str) -> bool {
    HARD_EXCLUDED_DIRS
        .iter()
        .any(|dir| dir.eq_ignore_ascii_case(name))
}

/// Whether a path is excluded from the **watch** by the hard-coded set
/// (PRD §4.3).
///
/// This is the half that needs no repository context: `.git/`, `node_modules/`,
/// `target/` and `dist/`, matched as whole path components at any depth. The
/// `.gitignore` half needs the ignore files themselves and so lives on
/// [`IgnoreSet::is_ignored`]; [`FsWatcher`] applies both.
///
/// `node_modules`, vendored and generated trees are *not* simply ignored by the
/// product: they are rendered as a single dull mass (PRD §8). This predicate
/// governs the watch only; the rendering classifier is `polis_repo::FileClass`,
/// which this crate deliberately does not depend on.
pub fn is_watch_excluded(path: &Path) -> bool {
    path.components()
        .filter_map(|c| c.as_os_str().to_str())
        .any(is_hard_excluded_name)
}

/// A Windows-specific startup self-check.
///
/// A deep `cwd` produces a 282-character transcript path on a machine with
/// `LongPathsEnabled = 0`. Rust's `std::fs` copes (verified); `notify` against
/// such a directory is **untested** and is carried forward as open item 6 in
/// `docs/DECISIONS.md`. PRD §16 should not discover this at runtime, so startup
/// actually reads a discovered path before advertising the session as watchable.
///
/// "Reads" is meant literally: a `metadata` call can be served from a cache, so
/// this opens a directory handle or a file handle, which is the operation that
/// actually fails on a long path.
pub fn self_check_long_paths(sample: &Path) -> bool {
    match fs::metadata(sample) {
        Ok(meta) if meta.is_dir() => fs::read_dir(sample).is_ok(),
        Ok(_) => fs::File::open(sample).is_ok(),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Debouncing
// ---------------------------------------------------------------------------

/// Coalesces per-path changes until a path has been quiet for the window.
///
/// Deliberately independent of `notify`, threads and clocks: it takes an
/// [`Instant`] rather than reading one, so its behaviour is exercisable without
/// a filesystem or a sleep.
#[derive(Debug)]
struct Debouncer {
    window: Duration,
    /// `BTreeMap` so a flush emits in a deterministic order (PRD §7.4).
    pending: BTreeMap<Key, PendingChange>,
    /// `notify`'s rename tracker -> the `from` half still waiting for its `to`.
    renames: BTreeMap<usize, PendingRename>,
}

#[derive(Debug, Clone, Copy)]
struct PendingChange {
    change: FsChange,
    last: Instant,
}

#[derive(Debug, Clone)]
struct PendingRename {
    from: Key,
    at: Instant,
}

impl Debouncer {
    fn new(window: Duration) -> Self {
        Self {
            window,
            pending: BTreeMap::new(),
            renames: BTreeMap::new(),
        }
    }

    /// Folds one observed change in, emitting immediately only for a rename
    /// whose two halves have both arrived.
    fn record(
        &mut self,
        key: Key,
        change: FsChange,
        tracker: Option<usize>,
        now: Instant,
        out: &mut Vec<FsEvent>,
    ) {
        match change {
            FsChange::RenamedFrom => match tracker {
                Some(id) => {
                    self.renames
                        .insert(id, PendingRename { from: key, at: now });
                }
                // No tracker means the backend cannot pair the halves, and a
                // `from` with no `to` is a deletion.
                None => self.merge(key, FsChange::Removed, now),
            },
            FsChange::RenamedTo => match tracker.and_then(|id| self.renames.remove(&id)) {
                Some(pending) => {
                    self.flush_key(&pending.from, out);
                    self.flush_key(&key, out);
                    out.push(FsEvent::Renamed {
                        from: pending.from,
                        to: key,
                    });
                }
                None => self.merge(key, FsChange::Created, now),
            },
            // `RenamedBoth` is resolved by the caller, which is the only place
            // that can see both paths of one event.
            other => self.merge(key, other, now),
        }
    }

    fn merge(&mut self, key: Key, incoming: FsChange, now: Instant) {
        if let Some(entry) = self.pending.get_mut(&key) {
            match coalesce(entry.change, incoming) {
                Some(change) => {
                    entry.change = change;
                    entry.last = now;
                }
                None => {
                    self.pending.remove(&key);
                }
            }
        } else {
            self.pending.insert(
                key,
                PendingChange {
                    change: incoming,
                    last: now,
                },
            );
        }
    }

    /// Emits one key's pending change right now, if it has one.
    fn flush_key(&mut self, key: &Key, out: &mut Vec<FsEvent>) {
        if let Some(entry) = self.pending.remove(key) {
            out.push(normalize::single_fs_event(entry.change, key.clone()));
        }
    }

    /// Emits every path that has been quiet for the window.
    fn flush(&mut self, now: Instant, out: &mut Vec<FsEvent>) -> usize {
        let before = out.len();
        let ripe: Vec<Key> = self
            .pending
            .iter()
            .filter(|(_, p)| {
                now.checked_duration_since(p.last)
                    .is_some_and(|elapsed| elapsed >= self.window)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in ripe {
            self.flush_key(&key, out);
        }
        // A `from` whose `to` never arrived really was a deletion.
        let stale: Vec<usize> = self
            .renames
            .iter()
            .filter(|(_, r)| {
                now.checked_duration_since(r.at)
                    .is_some_and(|elapsed| elapsed >= self.window)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            if let Some(pending) = self.renames.remove(&id) {
                out.push(FsEvent::Removed { path: pending.from });
            }
        }
        out.len() - before
    }

    /// Emits everything, regardless of quiet time. Used at shutdown.
    fn flush_all(&mut self, out: &mut Vec<FsEvent>) -> usize {
        let before = out.len();
        for (key, entry) in std::mem::take(&mut self.pending) {
            out.push(normalize::single_fs_event(entry.change, key));
        }
        for (_, pending) in std::mem::take(&mut self.renames) {
            out.push(FsEvent::Removed { path: pending.from });
        }
        out.len() - before
    }
}

/// How two changes to the same path inside one window combine.
fn coalesce(existing: FsChange, incoming: FsChange) -> Option<FsChange> {
    use FsChange::{Created, Modified, Removed};
    match (existing, incoming) {
        // A file written straight after being created is still a new building.
        (Created, Modified) => Some(Created),
        // Created and deleted inside one window is a temp file. Net nothing —
        // which is most of what debouncing an editor's save is for.
        (Created, Removed) => None,
        // Delete-then-create is how an atomic save lands. The building stays.
        (Removed, Created | Modified) => Some(Modified),
        (_, latest) => Some(latest),
    }
}

// ---------------------------------------------------------------------------
// The watcher
// ---------------------------------------------------------------------------

/// What the `notify` callback forwards to the debounce thread.
///
/// Errors travel the same queue as events so that every push onto the bus
/// happens on one thread, in one order.
#[derive(Debug)]
enum Raw {
    Event(Box<notify::Event>),
    Error(String),
}

/// State shared between the caller, the `notify` callback and the debouncer.
#[derive(Debug)]
struct Shared {
    mapper: Mutex<PathMapper>,
    ignores: Mutex<BTreeMap<WorktreeId, IgnoreSet>>,
    correlation: Mutex<CorrelationWindow>,
    degraded: Mutex<Option<String>>,
    rescan: AtomicBool,
    stopped: AtomicBool,
}

impl Shared {
    /// A watched path's layout key, or `None` when it is excluded or outside
    /// every registered root.
    fn resolve(&self, path: &Path) -> Option<Key> {
        if is_watch_excluded(path) {
            return None;
        }
        let (worktree, logical) = lock(&self.mapper).to_logical(path)?;
        let ignored = lock(&self.ignores)
            .get(&worktree)
            .is_some_and(|set| set.is_ignored(logical.as_str(), false));
        (!ignored).then_some((worktree, logical))
    }

    fn note_rescan(&self) {
        self.rescan.store(true, Ordering::Release);
    }
}

/// The `notify` watcher thread.
pub struct FsWatcher {
    watcher: Option<RecommendedWatcher>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    config: FsWatchConfig,
}

impl fmt::Debug for FsWatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `RecommendedWatcher` is not `Debug` on every platform, and the useful
        // state is the health anyway.
        f.debug_struct("FsWatcher")
            .field("running", &self.thread.is_some())
            .field("needs_rescan", &self.needs_rescan())
            .field("debounce", &self.config.debounce)
            .finish_non_exhaustive()
    }
}

impl FsWatcher {
    /// Starts watching a set of roots recursively.
    ///
    /// Watch every registered worktree, not only the primary: PRD §7.6 makes
    /// worktrees first-class, and the whole point is seeing two agents edit the
    /// same logical file on different branches. Each root is paired with the
    /// [`WorktreeId`] its events should be stamped with.
    pub fn start(
        roots: &[(WorktreeId, &Path)],
        sink: EventSink,
        mapper: PathMapper,
    ) -> anyhow::Result<Self> {
        Self::start_with(roots, sink, mapper, FsWatchConfig::default())
    }

    /// [`FsWatcher::start`] with explicit tuning.
    ///
    /// A root that cannot be watched degrades the channel rather than failing
    /// the start (ADR-0011); only being unable to watch *anything* is an error.
    pub fn start_with(
        roots: &[(WorktreeId, &Path)],
        sink: EventSink,
        mapper: PathMapper,
        config: FsWatchConfig,
    ) -> anyhow::Result<Self> {
        let shared = Arc::new(Shared {
            mapper: Mutex::new(mapper),
            ignores: Mutex::new(BTreeMap::new()),
            correlation: Mutex::new(CorrelationWindow::with_window(config.correlation_window)),
            degraded: Mutex::new(None),
            rescan: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        });
        let (raw_tx, raw_rx) = crossbeam_channel::bounded(config.raw_capacity.max(1));
        let mut watcher = build_watcher(raw_tx, Arc::clone(&shared))?;

        let mut live_roots = 0usize;
        for (id, root) in roots {
            match watcher.watch(root, RecursiveMode::Recursive) {
                Ok(()) => {
                    register_root(&shared, *id, root, config.max_walk_entries);
                    live_roots += 1;
                }
                Err(err) => {
                    let reason = format!("cannot watch {}: {err}", root.display());
                    *lock(&shared.degraded) = Some(reason.clone());
                    sink.push_control(ControlEvent::ChannelDegraded {
                        channel: Channel::Fs,
                        reason,
                    });
                }
            }
        }
        if live_roots == 0 {
            return Err(anyhow!(
                "no watchable root among {} candidate(s)",
                roots.len()
            ));
        }

        let thread = spawn_debouncer(raw_rx, Arc::clone(&shared), sink, config)?;
        Ok(Self {
            watcher: Some(watcher),
            shared,
            thread: Some(thread),
            config,
        })
    }

    /// Adds a worktree discovered after start (PRD §7.6).
    ///
    /// Worktrees appear mid-session — `SessionStart` with a new `cwd`, or
    /// `CwdChanged`. Never from a `WorktreeCreate` hook, which Polis must not
    /// register (ADR-0002).
    pub fn watch_worktree(&mut self, id: WorktreeId, root: &Path) -> anyhow::Result<()> {
        let watcher = self
            .watcher
            .as_mut()
            .context("the watcher has already been shut down")?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .with_context(|| format!("cannot watch {}", root.display()))?;
        register_root(&self.shared, id, root, self.config.max_walk_entries);
        Ok(())
    }

    /// Records an **authoritative** tool call for the ±2 s correlation window
    /// (PRD §4.3).
    ///
    /// Fed from `PreToolUse` or an OTel tool span. Anchoring on `touch.at`,
    /// which is `EventMeta::observed`, is not a detail: a window keyed on
    /// transcript timestamps mis-attributes on a fifth of real sessions
    /// (ADR-0014).
    pub fn observe_tool_call(&self, touch: ToolTouch) {
        lock(&self.shared.correlation).observe(touch);
    }

    /// Correlates a filesystem write against the recorded calls.
    ///
    /// The result is [`AdvisoryAttribution`] and there is no authoritative form
    /// of it. Do not let it drive an alert (PRD §17).
    pub fn advisory_attribution(&self, path: &Key, at: Instant) -> Option<AdvisoryAttribution> {
        lock(&self.shared.correlation).correlate(path, at)
    }

    /// True once the watcher has reported a queue overflow and the tree needs a
    /// re-scan. Ignoring overflow silently desynchronises the city from disk.
    ///
    /// Clearing it is the caller's job, after the re-scan actually happened —
    /// see [`FsWatcher::clear_rescan`].
    pub fn needs_rescan(&self) -> bool {
        self.shared.rescan.load(Ordering::Acquire)
    }

    /// Acknowledges a completed re-scan.
    pub fn clear_rescan(&mut self) {
        self.shared.rescan.store(false, Ordering::Release);
    }

    /// True when a path would be excluded from the watch — the hard-coded set
    /// plus this repository's `.gitignore` files.
    pub fn is_excluded(&self, path: &Path) -> bool {
        self.shared.resolve(path).is_none()
    }

    /// Requests shutdown and joins the watcher thread.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        // Dropping the watcher drops the callback, which drops the sender, which
        // disconnects the debouncer's queue. The stop flag covers the platforms
        // where that ordering is less obliging.
        drop(self.watcher.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for FsWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

impl IngestSource for FsWatcher {
    fn channel(&self) -> Channel {
        Channel::Fs
    }

    fn health(&self) -> SourceHealth {
        if self.thread.is_none() {
            return SourceHealth::Stopped {
                reason: "watcher shut down".to_owned(),
            };
        }
        if let Some(reason) = lock(&self.shared.degraded).clone() {
            return SourceHealth::Degraded { reason };
        }
        if self.needs_rescan() {
            return SourceHealth::Degraded {
                reason: "watcher queue overflowed; a tree re-scan is outstanding".to_owned(),
            };
        }
        SourceHealth::Running
    }

    fn shutdown(self: Box<Self>) {
        (*self).shutdown();
    }
}

/// Builds the `notify` watcher, forwarding everything onto one queue.
fn build_watcher(raw: Sender<Raw>, shared: Arc<Shared>) -> anyhow::Result<RecommendedWatcher> {
    let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let message = match result {
            Ok(event) => Raw::Event(Box::new(event)),
            Err(err) => Raw::Error(err.to_string()),
        };
        // Never block here: this runs on `notify`'s thread, and a stalled
        // callback is how the *OS* queue overflows next.
        if raw.try_send(message).is_err() {
            shared.note_rescan();
        }
    })
    .context("cannot create a filesystem watcher")?;
    Ok(watcher)
}

/// Registers a root with the mapper (if it does not know it) and reads its
/// ignore files.
fn register_root(shared: &Arc<Shared>, id: WorktreeId, root: &Path, max_entries: usize) {
    {
        let mut mapper = lock(&shared.mapper);
        if mapper.worktree_root(id).is_none() {
            let _ = mapper.add_worktree(id, root);
        }
    }
    let ignores = IgnoreSet::load(root, max_entries);
    lock(&shared.ignores).insert(id, ignores);
}

/// Starts the debounce thread.
fn spawn_debouncer(
    raw: Receiver<Raw>,
    shared: Arc<Shared>,
    sink: EventSink,
    config: FsWatchConfig,
) -> anyhow::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("polis-fswatch".to_owned())
        .spawn(move || debounce_loop(&raw, &shared, &sink, config))
        .context("cannot start the filesystem watcher thread")
}

/// The debounce thread's whole life.
fn debounce_loop(
    raw: &Receiver<Raw>,
    shared: &Arc<Shared>,
    sink: &EventSink,
    config: FsWatchConfig,
) {
    let tick = (config.debounce / 4).max(Duration::from_millis(5));
    let mut debouncer = Debouncer::new(config.debounce);
    let mut out: Vec<FsEvent> = Vec::new();
    loop {
        match raw.recv_timeout(tick) {
            Ok(Raw::Event(event)) => ingest_raw(&event, shared, &mut debouncer, &mut out),
            Ok(Raw::Error(reason)) => {
                // A watcher error means events were missed, which is the same
                // problem an OS queue overflow causes.
                shared.note_rescan();
                *lock(&shared.degraded) = Some(reason.clone());
                sink.push_control(ControlEvent::ChannelDegraded {
                    channel: Channel::Fs,
                    reason,
                });
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        debouncer.flush(Instant::now(), &mut out);
        emit(&mut out, sink);
        if shared.stopped.load(Ordering::Acquire) {
            break;
        }
    }
    debouncer.flush_all(&mut out);
    emit(&mut out, sink);
}

/// Folds one raw `notify` event into the debouncer.
fn ingest_raw(
    event: &notify::Event,
    shared: &Arc<Shared>,
    debouncer: &mut Debouncer,
    out: &mut Vec<FsEvent>,
) {
    if event.need_rescan() {
        shared.note_rescan();
        out.push(FsEvent::RescanRequired);
        return;
    }
    let Some(change) = normalize::fs_change(event.kind) else {
        return;
    };
    let now = Instant::now();
    let mapped: Vec<Key> = event
        .paths
        .iter()
        .filter_map(|path| shared.resolve(path))
        .collect();
    if change == FsChange::RenamedBoth {
        if let [from, to, ..] = mapped.as_slice() {
            debouncer.flush_key(from, out);
            debouncer.flush_key(to, out);
            out.push(FsEvent::Renamed {
                from: from.clone(),
                to: to.clone(),
            });
            return;
        }
        // One usable path out of a rename is not enough to relocate a building,
        // and demolishing one on a guess is worse than saying "it was touched".
        for key in mapped {
            debouncer.record(key, FsChange::Modified, None, now, out);
        }
        return;
    }
    for key in mapped {
        debouncer.record(key, change, event.tracker(), now, out);
    }
}

/// Pushes and clears the pending output.
fn emit(out: &mut Vec<FsEvent>, sink: &EventSink) {
    for event in out.drain(..) {
        sink.push(normalize::fs_bus_event(event));
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use polis_events::{Payload, SessionId};

    use super::*;

    // -- .gitignore ------------------------------------------------------

    fn ignores(files: &[(&str, &str)]) -> IgnoreSet {
        let mut set = IgnoreSet::new();
        for (base, contents) in files {
            set.add_file(base, contents);
        }
        set
    }

    #[test]
    fn the_hard_coded_set_is_a_component_match_not_a_substring_match() {
        assert!(is_watch_excluded(Path::new("/repo/.git/HEAD")));
        assert!(is_watch_excluded(Path::new("/repo/a/node_modules/x/i.js")));
        assert!(is_watch_excluded(Path::new("/repo/target/debug/x")));
        assert!(is_watch_excluded(Path::new("/repo/web/dist/app.js")));
        // A substring test would exclude all four of these, and three of them
        // are ordinary source files.
        assert!(!is_watch_excluded(Path::new("/repo/src/targeting.rs")));
        assert!(!is_watch_excluded(Path::new("/repo/src/distance.rs")));
        assert!(!is_watch_excluded(Path::new("/repo/.gitignore")));
        assert!(!is_watch_excluded(Path::new("/repo/src/git.rs")));
    }

    #[test]
    fn gitignore_basics_match_git_semantics() {
        let set = ignores(&[(
            "",
            "# a comment\n\
             \n\
             *.log\n\
             /only-at-root.txt\n\
             build/\n\
             docs/*.md\n",
        )]);
        assert!(set.is_ignored("a.log", false));
        // No slash in the pattern means basename at any depth.
        assert!(set.is_ignored("src/deep/b.log", false));
        assert!(!set.is_ignored("a.log.rs", false));

        // A leading slash anchors to the ignore file's directory.
        assert!(set.is_ignored("only-at-root.txt", false));
        assert!(!set.is_ignored("src/only-at-root.txt", false));

        // A trailing slash is directory-only, and excluding a directory
        // excludes everything under it.
        assert!(set.is_ignored("build", true));
        assert!(!set.is_ignored("build", false));
        assert!(set.is_ignored("build/out.o", false));
        assert!(set.is_ignored("src/build/out.o", false));

        // A slash mid-pattern anchors, and `*` does not cross a separator.
        assert!(set.is_ignored("docs/readme.md", false));
        assert!(!set.is_ignored("docs/deep/readme.md", false));
        assert!(!set.is_ignored("src/docs/readme.md", false));
    }

    #[test]
    fn negation_re_includes_and_order_decides() {
        let set = ignores(&[("", "*.log\n!keep.log\n")]);
        assert!(set.is_ignored("noise.log", false));
        assert!(
            !set.is_ignored("keep.log", false),
            "the last matching rule wins"
        );

        // Reversed, the negation is overridden by the later blanket rule.
        let set = ignores(&[("", "!keep.log\n*.log\n")]);
        assert!(set.is_ignored("keep.log", false));

        // Git cannot re-include a file whose parent directory is excluded, and
        // a matcher that only tested the full path would get this wrong.
        let set = ignores(&[("", "vendor/\n!vendor/keep.rs\n")]);
        assert!(set.is_ignored("vendor/keep.rs", false));
    }

    #[test]
    fn nested_ignore_files_apply_to_their_own_subtree_and_win() {
        let set = ignores(&[
            ("", "*.tmp\n"),
            // A deeper file may re-include what a shallower one excluded.
            ("src", "!important.tmp\nlocal.rs\n"),
        ]);
        assert!(set.is_ignored("a.tmp", false));
        assert!(set.is_ignored("src/other.tmp", false));
        assert!(
            !set.is_ignored("src/important.tmp", false),
            "the deeper ignore file overrides the shallower one"
        );
        // The nested file's own rules do not escape its directory.
        assert!(set.is_ignored("src/local.rs", false));
        assert!(!set.is_ignored("local.rs", false));
        assert!(!set.is_ignored("tests/local.rs", false));
    }

    #[test]
    fn double_star_and_character_classes_compile() {
        let set = ignores(&[(
            "",
            "**/generated\n\
             src/**/snapshot.json\n\
             tmp/**\n\
             ?ache\n\
             [Bb]in/\n\
             file[!0-9].txt\n",
        )]);
        // `**/` is any depth, including zero.
        assert!(set.is_ignored("generated", true));
        assert!(set.is_ignored("a/b/generated", true));
        assert!(set.is_ignored("src/x/y/snapshot.json", false));
        assert!(set.is_ignored("src/snapshot.json", false));
        assert!(!set.is_ignored("other/snapshot.json", false));
        assert!(set.is_ignored("tmp/a/b.txt", false));

        assert!(set.is_ignored("cache", false));
        assert!(!set.is_ignored("ccache", false));

        assert!(set.is_ignored("Bin", true));
        assert!(set.is_ignored("bin/x", false));

        assert!(set.is_ignored("filea.txt", false));
        assert!(!set.is_ignored("file3.txt", false), "the class is negated");
    }

    #[test]
    fn escaped_and_whitespace_lines_do_not_become_stray_rules() {
        let set = ignores(&[(
            "",
            "trailing   \n\
             escaped\\ \n\
             \\#literal-hash\n\
             \\!literal-bang\n",
        )]);
        assert!(set.is_ignored("trailing", false));
        assert!(!set.is_ignored("trailing   ", false));
        assert!(set.is_ignored("escaped ", false));
        assert!(set.is_ignored("#literal-hash", false));
        assert!(set.is_ignored("!literal-bang", false));
        // A comment is not a rule and neither is a blank line.
        assert_eq!(ignores(&[("", "# x\n\n   \n")]).rule_count(), 0);
    }

    #[test]
    fn a_gitignore_is_read_from_disk_and_prunes_the_walk() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(root.path().join("src/deep")).expect("dirs");
        fs::create_dir_all(root.path().join("node_modules/pkg")).expect("dirs");
        fs::write(root.path().join(".gitignore"), "*.log\nvendor/\n").expect("root ignore");
        fs::create_dir_all(root.path().join("src/vendor")).expect("dirs");
        fs::write(root.path().join("src/.gitignore"), "!keep.log\n").expect("nested ignore");

        let set = IgnoreSet::load(root.path(), 10_000);
        assert!(set.is_ignored("a.log", false));
        assert!(!set.is_ignored("src/keep.log", false));
        assert!(set.is_ignored("src/vendor/x.rs", false));
        // The hard-coded set prunes the walk before any ignore file is read.
        assert!(is_watch_excluded(
            &root.path().join("node_modules/pkg/i.js")
        ));
    }

    #[test]
    fn a_pathological_ignore_file_is_compiled_without_panicking() {
        for line in [
            "[",
            "[]",
            "[!]",
            "***/***",
            "a\\",
            "!",
            "/",
            "//",
            "\\",
            &"a/".repeat(500),
            &"*".repeat(2_000),
        ] {
            let set = ignores(&[("", line)]);
            let _ = set.is_ignored("a/b/c.rs", false);
            let _ = set.is_ignored("", true);
        }
    }

    // -- debounce --------------------------------------------------------

    fn key(path: &str) -> Key {
        (
            WorktreeId::PRIMARY,
            LogicalPath::new(path).expect("a logical path"),
        )
    }

    #[test]
    fn a_burst_of_writes_becomes_one_event_after_the_quiet_period() {
        let window = Duration::from_millis(200);
        let mut deb = Debouncer::new(window);
        let t0 = Instant::now();
        let mut out = Vec::new();

        // An editor's save: five events in 40 ms.
        for step in 0..5 {
            let at = t0 + Duration::from_millis(step * 10);
            deb.record(key("src/a.rs"), FsChange::Modified, None, at, &mut out);
            assert_eq!(deb.flush(at, &mut out), 0, "nothing may leave early");
        }
        assert!(out.is_empty());

        // Still inside the window.
        assert_eq!(deb.flush(t0 + Duration::from_millis(199), &mut out), 0);
        // Quiet for the window: exactly one event, not five.
        assert_eq!(deb.flush(t0 + Duration::from_millis(300), &mut out), 1);
        assert!(matches!(out[0], FsEvent::Modified { .. }));
    }

    #[test]
    fn coalescing_tells_a_temp_file_from_an_atomic_save() {
        let window = Duration::from_millis(100);
        let t0 = Instant::now();
        let late = t0 + Duration::from_secs(1);

        // Create then write is still a creation.
        let mut deb = Debouncer::new(window);
        let mut out = Vec::new();
        deb.record(key("new.rs"), FsChange::Created, None, t0, &mut out);
        deb.record(key("new.rs"), FsChange::Modified, None, t0, &mut out);
        deb.flush(late, &mut out);
        assert!(matches!(out.as_slice(), [FsEvent::Created { .. }]));

        // Create then delete inside the window is a temp file: net nothing.
        let mut deb = Debouncer::new(window);
        let mut out = Vec::new();
        deb.record(key("x.tmp"), FsChange::Created, None, t0, &mut out);
        deb.record(key("x.tmp"), FsChange::Removed, None, t0, &mut out);
        assert_eq!(deb.flush(late, &mut out), 0);
        assert!(out.is_empty());

        // Delete then create is an atomic save: the building stays.
        let mut deb = Debouncer::new(window);
        let mut out = Vec::new();
        deb.record(key("saved.rs"), FsChange::Removed, None, t0, &mut out);
        deb.record(key("saved.rs"), FsChange::Created, None, t0, &mut out);
        deb.flush(late, &mut out);
        assert!(matches!(out.as_slice(), [FsEvent::Modified { .. }]));
    }

    #[test]
    fn rename_halves_are_paired_by_tracker_and_time_out_as_deletions() {
        let window = Duration::from_millis(100);
        let t0 = Instant::now();

        let mut deb = Debouncer::new(window);
        let mut out = Vec::new();
        deb.record(key("old.rs"), FsChange::RenamedFrom, Some(7), t0, &mut out);
        assert!(out.is_empty(), "a half rename emits nothing yet");
        deb.record(key("new.rs"), FsChange::RenamedTo, Some(7), t0, &mut out);
        match out.as_slice() {
            [FsEvent::Renamed { from, to }] => {
                assert_eq!(from.1.as_str(), "old.rs");
                assert_eq!(to.1.as_str(), "new.rs");
            }
            other => panic!("expected one rename, got {other:?}"),
        }

        // A `from` whose `to` never arrives really was a deletion.
        let mut deb = Debouncer::new(window);
        let mut out = Vec::new();
        deb.record(key("gone.rs"), FsChange::RenamedFrom, Some(9), t0, &mut out);
        assert_eq!(deb.flush(t0 + Duration::from_secs(1), &mut out), 1);
        assert!(matches!(out.as_slice(), [FsEvent::Removed { .. }]));

        // With no tracker the backend cannot pair them at all.
        let mut deb = Debouncer::new(window);
        let mut out = Vec::new();
        deb.record(key("gone.rs"), FsChange::RenamedFrom, None, t0, &mut out);
        deb.flush(t0 + Duration::from_secs(1), &mut out);
        assert!(matches!(out.as_slice(), [FsEvent::Removed { .. }]));
    }

    #[test]
    fn a_flush_emits_in_a_deterministic_order() {
        let mut deb = Debouncer::new(Duration::ZERO);
        let t0 = Instant::now();
        let mut out = Vec::new();
        for path in ["z.rs", "a.rs", "m/n.rs", "M/a.rs"] {
            deb.record(key(path), FsChange::Modified, None, t0, &mut out);
        }
        deb.flush(t0 + Duration::from_secs(1), &mut out);
        let order: Vec<String> = out
            .iter()
            .map(|e| match e {
                FsEvent::Modified { path } => path.1.as_str().to_owned(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        let mut sorted = order.clone();
        sorted.sort_by_key(|p| p.to_ascii_lowercase());
        assert_eq!(order, sorted, "PRD §7.4 forbids iteration-order surprises");
    }

    // -- attribution -----------------------------------------------------

    fn touch(thread: &str, path: &str, at: Instant) -> ToolTouch {
        ToolTouch {
            thread: ThreadId::of_session(SessionId::new(thread)),
            worker: None,
            tool: ToolKind::Edit,
            path: key(path),
            at,
        }
    }

    /// PRD §4.3's window, and PRD §17's rule that it may never drive an alert.
    #[test]
    fn correlation_is_within_two_seconds_and_is_flagged_advisory() {
        let mut window = CorrelationWindow::new();
        let t0 = Instant::now() + Duration::from_secs(10);
        window.observe(touch("s-1", "src/a.rs", t0));

        // Inside ±2 s, on either side of the call.
        for offset in [
            Duration::from_millis(0),
            Duration::from_millis(1_900),
            Duration::from_secs(2),
        ] {
            let found = window
                .correlate(&key("src/a.rs"), t0 + offset)
                .expect("inside the window");
            assert!(
                !found.is_authoritative(),
                "an FS-derived attribution is never authoritative (ADR-0003)"
            );
            assert_eq!(found.advisory_thread().as_str(), "s-1");
            assert_eq!(found.advisory_tool(), &ToolKind::Edit);
            assert!(!found.is_ambiguous());
            assert!(found.lag() <= Duration::from_secs(2));
        }
        let before = window
            .correlate(
                &key("src/a.rs"),
                t0.checked_sub(Duration::from_millis(1_500))
                    .expect("before"),
            )
            .expect("a write just before the call still correlates");
        assert!(!before.is_authoritative());

        // Outside the window, and on a different path.
        assert!(window
            .correlate(&key("src/a.rs"), t0 + Duration::from_millis(2_001))
            .is_none());
        assert!(window.correlate(&key("src/b.rs"), t0).is_none());
    }

    #[test]
    fn two_threads_on_one_path_produce_an_ambiguous_advisory_never_a_verdict() {
        let mut window = CorrelationWindow::new();
        let t0 = Instant::now() + Duration::from_secs(10);
        window.observe(touch("s-1", "src/a.rs", t0));
        window.observe(touch("s-2", "src/a.rs", t0 + Duration::from_millis(100)));

        let found = window
            .correlate(&key("src/a.rs"), t0 + Duration::from_millis(110))
            .expect("something correlates");
        assert!(found.is_ambiguous());
        assert_eq!(found.candidate_threads(), 2);
        // The nearest call wins, but the result is still only advisory — this
        // is exactly the case PRD §11.3 sends to `PreToolUse` instead.
        assert_eq!(found.advisory_thread().as_str(), "s-2");
        assert!(!found.is_authoritative());
    }

    #[test]
    fn the_window_forgets_calls_that_fell_out_of_it() {
        let mut window = CorrelationWindow::with_window(Duration::from_millis(500));
        let t0 = Instant::now() + Duration::from_secs(10);
        window.observe(touch("s-1", "src/a.rs", t0));
        assert_eq!(window.len(), 1);
        window.observe(touch("s-2", "src/b.rs", t0 + Duration::from_secs(5)));
        assert_eq!(window.len(), 1, "the stale call was evicted");
        assert!(!window.is_empty());
    }

    // -- the live watcher ------------------------------------------------

    #[test]
    fn a_real_write_reaches_the_bus_with_no_attribution_on_it() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(root.path().join("src")).expect("src");
        fs::write(root.path().join(".gitignore"), "*.log\n").expect("ignore file");

        let (sink, source) = crate::bus::channel(1_024);
        let mapper = PathMapper::new(root.path()).expect("mappable root");
        let config = FsWatchConfig {
            debounce: Duration::from_millis(50),
            ..FsWatchConfig::default()
        };
        let watcher =
            FsWatcher::start_with(&[(WorktreeId::PRIMARY, root.path())], sink, mapper, config)
                .expect("the watcher starts");
        assert!(watcher.health().is_healthy());

        // An ignored file and a watched one, written together.
        let mut file = fs::File::create(root.path().join("src/a.rs")).expect("create");
        file.write_all(b"fn main() {}").expect("write");
        file.sync_all().expect("sync");
        drop(file);
        fs::write(root.path().join("noise.log"), b"noise").expect("write log");

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            let mut batch = Vec::new();
            source.drain(&mut batch);
            for event in batch {
                assert!(event.meta.session.is_none(), "Channel C never attributes");
                assert!(event.meta.thread.is_none());
                assert!(event.meta.worker.is_none());
                if let Payload::Fs(fs_event) = &event.payload {
                    seen.push(fs_event.clone());
                }
            }
            if seen.iter().any(|e| mentions(e, "src/a.rs")) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        watcher.shutdown();

        assert!(
            seen.iter().any(|e| mentions(e, "src/a.rs")),
            "the written file never arrived: {seen:?}"
        );
        assert!(
            !seen.iter().any(|e| mentions(e, "noise.log")),
            ".gitignore governs the watch: {seen:?}"
        );
    }

    fn mentions(event: &FsEvent, path: &str) -> bool {
        let matches_key = |k: &Key| k.1.as_str() == path;
        match event {
            FsEvent::Created { path } | FsEvent::Modified { path } | FsEvent::Removed { path } => {
                matches_key(path)
            }
            FsEvent::Renamed { from, to } => matches_key(from) || matches_key(to),
            _ => false,
        }
    }

    #[test]
    fn a_rescan_flag_is_surfaced_and_only_cleared_deliberately() {
        let root = tempfile::tempdir().expect("tempdir");
        let (sink, _source) = crate::bus::channel(16);
        let mapper = PathMapper::new(root.path()).expect("mappable root");
        let mut watcher = FsWatcher::start(&[(WorktreeId::PRIMARY, root.path())], sink, mapper)
            .expect("the watcher starts");
        assert!(!watcher.needs_rescan());
        assert!(watcher.health().is_healthy());

        watcher.shared.note_rescan();
        assert!(watcher.needs_rescan());
        assert!(
            !watcher.health().is_healthy(),
            "an outstanding re-scan is a degraded channel, not a healthy one"
        );
        watcher.clear_rescan();
        assert!(!watcher.needs_rescan());
        watcher.shutdown();
    }

    #[test]
    fn an_unwatchable_root_is_an_error_rather_than_a_silent_no_op() {
        let (sink, _source) = crate::bus::channel(16);
        let root = Path::new(if cfg!(windows) {
            "C:\\definitely\\not\\here\\polis"
        } else {
            "/definitely/not/here/polis"
        });
        let mapper = PathMapper::new(root).expect("mappable root");
        let started = FsWatcher::start(&[(WorktreeId::PRIMARY, root)], sink, mapper);
        assert!(started.is_err(), "an empty watch set must not look healthy");
    }

    #[test]
    fn the_long_path_self_check_actually_opens_something() {
        let root = tempfile::tempdir().expect("tempdir");
        assert!(self_check_long_paths(root.path()));
        let file = root.path().join("a.txt");
        fs::write(&file, b"x").expect("write");
        assert!(self_check_long_paths(&file));
        assert!(!self_check_long_paths(&root.path().join("missing")));
    }

    /// The debouncer is the only thing between `notify`'s thread and the bus.
    /// A compiler writing a target tree is thousands of raw events over a few
    /// hundred paths, and the bus must not see thousands.
    #[test]
    fn a_flood_over_few_paths_collapses_to_far_fewer_events() {
        const EVENTS: u64 = 5_000;
        const PATHS: u64 = 100;

        let mut deb = Debouncer::new(Duration::from_millis(10));
        let t0 = Instant::now();
        let mut out = Vec::new();
        let mut emitted = 0usize;
        for i in 0..EVENTS {
            let at = t0 + Duration::from_micros(i * 10);
            let path = format!("gen/{}.rs", i % PATHS);
            deb.record(key(&path), FsChange::Modified, None, at, &mut out);
            emitted += deb.flush(at, &mut out);
            out.clear();
        }
        deb.flush_all(&mut out);
        emitted += out.len();
        assert!(
            emitted < usize::try_from(EVENTS).expect("fits") / 4,
            "the debouncer coalesced almost nothing: {emitted} events survived"
        );
        assert!(emitted >= usize::try_from(PATHS).expect("fits"));
    }
}
