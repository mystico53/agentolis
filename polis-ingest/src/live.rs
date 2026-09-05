//! Live session discovery — every agent in a repository, with zero setup
//! (PRD §4.4, §15 M3).
//!
//! > *"i want to see all agents active in a repository on the machine"*
//!
//! Note what that asks for and what the rest of the stack assumed. `polis run`
//! can only ever show the one agent it launched itself, and hooks and telemetry
//! both need a configuration step before a single event exists. But **every**
//! Claude Code session on this machine, however it was started, appends to a
//! JSONL transcript under `~/.claude/projects`, and every record carries the
//! `cwd` it was written from. Presence, activity and attribution for every agent
//! in a repository are therefore obtainable from files that are already there,
//! with no hooks, no environment variables and no wrapper process.
//!
//! That is what this module does. [`crate::transcript::ProjectsTailer`] already
//! follows every session under a projects directory; this adds the three things
//! that turns following into *watching a repository*:
//!
//! 1. **A repository filter.** A [`Scope`] built from the watched checkout's
//!    [`PathMapper`], so a session in another repo is *reported* rather than
//!    either silently ignored or silently drawn onto the wrong city.
//! 2. **A liveness gate.** A machine with a year of history has hundreds of
//!    sessions, and `ProjectsTailer` holds an open tail on every one of them:
//!    at [`crate::transcript::POLL_INTERVAL`] that is a thousand `open` and
//!    `read_dir` calls a second on a machine where nothing is happening, against
//!    PRD §13.1's *"idle CPU < 2% of one core"*. Here a session is tailed only
//!    while it is [`Activity::Working`] or [`Activity::Idle`]; the rest are one
//!    `metadata` call every [`crate::transcript::RESCAN_EVERY`] polls.
//! 3. **A roster.** [`Roster`] is the answer to "which agents are working in
//!    this repository right now", as a value the status bar, `polis doctor` and
//!    `polis watch --list` all read, so none of them can drift from the others.
//!
//! # What "active" can and cannot be known from a transcript
//!
//! There is **no session-end record** in the JSONL format — `docs/verified/`
//! `jsonl-schema.md` enumerates all 19 record types and none of them closes a
//! session. So the three states here are defined by *silence*, and the middle
//! one is honestly ambiguous:
//!
//! | State | Means | Certainty |
//! |---|---|---|
//! | [`Activity::Working`] | appended to within [`WORKING_WINDOW`] | the agent is doing something now |
//! | [`Activity::Idle`] | quiet, but for less than [`LIVE_WINDOW`] | waiting for you, **or** already closed — indistinguishable |
//! | [`Activity::Dormant`] | quiet for longer than [`LIVE_WINDOW`] | history |
//!
//! Only Channel B's `Stop` and `SessionEnd` hooks can separate the two halves of
//! `Idle`, which is exactly why `polis connect` remains worth doing on top of a
//! zero-setup watch — and why the status area must say which channels are up.
//!
//! # Catching up without a flood
//!
//! A session that was already running when the window opened has history the
//! operator wants: the last few minutes of it *is* the picture of what the agent
//! is doing. Reading the whole file is what
//! [`crate::transcript::TranscriptTailer::start_discovering`] rightly refuses to
//! do — a 23 MB transcript times hundreds of sessions is a flood, not a rebuild
//! — so this reads a bounded tail instead: the last [`BACKFILL_BYTES`] of each
//! file of each **in-scope, live** session, cut at the first record boundary so
//! no half-line is ever parsed. Sessions that appear *after* the watch starts
//! are read whole, because they start empty.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use polis_events::{Event, PathMapper, SessionId};

use crate::transcript::{
    discover_project_dirs, project_dir_cwd, session_files, sessions_in, SessionTailer, StartAt,
};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Appended to within this, and the agent is doing something *right now*.
///
/// A working agent writes a record per tool call and per tool result, and at the
/// median that is one every few seconds. **It is not several a second at the
/// tail, and 45 s does not survive a slow `Bash` call** — a shell call writes
/// nothing at all between its `tool_use` block and its `tool_result`, and on
/// this machine's transcripts (65 652 settled calls) 3.3% of calls take longer
/// than 60 s, the longest 1 724 s. The transcript is silent for the whole of
/// each one.
///
/// The 45 s stands anyway, because **this constant does not decide what the map
/// paints.** It labels a session in the roster, and a session that drops to
/// [`Activity::Idle`] is still tailed until [`LIVE_WINDOW`] — nothing stops
/// flowing, nothing is dropped. The status on the map is
/// `polis_world::ThreadStatus`, and the in-flight call is accounted for there,
/// by `polis_world::IN_FLIGHT_MAX`.
pub const WORKING_WINDOW: Duration = Duration::from_secs(45);

/// Quiet for longer than this, and the session is treated as history.
///
/// It is not proof the session ended — nothing in the transcript is — so this is
/// the point at which Polis stops paying to follow it, not the point at which it
/// claims the agent went away.
pub const LIVE_WINDOW: Duration = Duration::from_mins(8);

/// Most bytes of an already-running session's history read at attach time, per
/// file.
///
/// Enough to carry the last several minutes of a busy session onto the map the
/// instant the window opens; small enough that attaching to a repository with
/// three live agents is a few megabytes, not a few hundred.
pub const BACKFILL_BYTES: u64 = 2 * 1024 * 1024;

/// Most sessions followed at once.
///
/// A ceiling rather than a limit that is expected to bind: the liveness gate
/// already keeps this at the handful of sessions actually running. It exists so
/// that a pathological projects directory — a hundred agents, or a clock that
/// jumped — cannot turn discovery into an unbounded number of open files.
pub const MAX_TAILED: usize = 64;

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

/// Which repository's agents a watch is about.
#[derive(Debug, Clone)]
pub enum Scope {
    /// Only sessions whose `cwd` lies inside one of this mapper's roots.
    ///
    /// A [`PathMapper`] rather than a `PathBuf` for two reasons: it already does
    /// component-wise, ASCII-case-insensitive prefix matching, so the classic
    /// `\repo` / `\repo-backup` false positive cannot happen; and it carries
    /// every registered **worktree**, so an agent working in a second checkout
    /// of the same repository is in scope, which PRD §7.6 requires.
    Repo(PathMapper),
    /// Every session on the machine, whatever repository it is in.
    Everything,
}

/// Whether a session belongs to the watched repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// Its `cwd` is inside the watched checkout.
    Inside,
    /// Its `cwd` is somewhere else. Reported, never drawn.
    Outside,
    /// No record has carried a `cwd` yet — a session whose first line has not
    /// landed. Not an error; ask again next scan.
    Unknown,
}

impl Scope {
    /// Where a session with this `cwd` sits relative to the watch.
    pub fn fit(&self, cwd: Option<&str>) -> Fit {
        match (self, cwd) {
            (Self::Everything, Some(_)) => Fit::Inside,
            (_, None) => Fit::Unknown,
            (Self::Repo(mapper), Some(cwd)) => {
                if mapper.to_logical_str(cwd).is_some() {
                    Fit::Inside
                } else {
                    Fit::Outside
                }
            }
        }
    }

    /// True when this watch is not restricted to one repository.
    pub fn is_everything(&self) -> bool {
        matches!(self, Self::Everything)
    }

    /// The checkout being watched, for a message. `None` for [`Scope::Everything`].
    pub fn repo(&self) -> Option<&str> {
        match self {
            Self::Everything => None,
            Self::Repo(mapper) => mapper.roots().next().map(|(_, root)| root),
        }
    }
}

// ---------------------------------------------------------------------------
// The roster
// ---------------------------------------------------------------------------

/// What a session is doing, inferred from when its files last grew.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Activity {
    /// Appended to within [`WORKING_WINDOW`]. An agent is working.
    Working,
    /// Quiet for less than [`LIVE_WINDOW`]. Waiting for its operator, or closed
    /// — the transcript cannot tell those apart; a `Stop` hook can.
    Idle,
    /// Quiet for longer than [`LIVE_WINDOW`]. History; not followed.
    Dormant,
}

impl Activity {
    /// The one-word form used in every human-readable rendering.
    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Dormant => "dormant",
        }
    }

    /// Whether a session in this state is worth following.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Working | Self::Idle)
    }
}

/// One session as the roster sees it.
#[derive(Debug, Clone)]
pub struct LiveSession {
    /// The session id — the main transcript's file stem.
    pub session: SessionId,
    /// The `<munged-cwd>/<session-id>` sidecar directory. Need not exist: it is
    /// created only once the session spawns a subagent.
    pub session_dir: PathBuf,
    /// The main transcript, `<munged-cwd>/<session-id>.jsonl`.
    pub transcript: PathBuf,
    /// The `<munged-cwd>` directory it lives under.
    pub project_dir: PathBuf,
    /// The working directory its records report, when one has been read.
    pub cwd: Option<String>,
    /// Where it sits relative to the watched repository.
    pub fit: Fit,
    /// Size of the main transcript.
    pub bytes: u64,
    /// When the session's files last grew.
    pub modified: Option<SystemTime>,
    /// How long it has been quiet, when that can be measured.
    pub quiet_for: Option<Duration>,
    /// Working, idle or dormant.
    pub activity: Activity,
    /// Subagent transcripts under the sidecar directory. Counted only for
    /// sessions being followed — for the rest it would cost a recursive
    /// `read_dir` per session per scan to learn a number nothing reads.
    pub subagents: usize,
    /// Whether Polis is following this session's files right now.
    pub tailing: bool,
    /// Whether it appeared *after* the watch started. The common case an
    /// operator actually exercises: open the map, then start an agent.
    pub started_while_watching: bool,
}

impl LiveSession {
    /// A one-line rendering for a terminal or a status rail.
    pub fn line(&self) -> String {
        let location = match (&self.cwd, self.fit) {
            (_, Fit::Unknown) => "repository not yet known".to_owned(),
            (Some(cwd), _) => cwd.clone(),
            (None, _) => "unknown".to_owned(),
        };
        let quiet = match self.quiet_for {
            Some(d) if self.activity != Activity::Working => {
                format!(", quiet for {}", short_duration(d))
            }
            _ => String::new(),
        };
        let subagents = if self.subagents > 0 {
            format!(", {} subagents", self.subagents)
        } else {
            String::new()
        };
        format!(
            "{}  {}{}{}  {}",
            self.session,
            self.activity.label(),
            quiet,
            subagents,
            location
        )
    }
}

/// Every session Polis can see, and what each is doing (PRD §15 M3).
///
/// Rebuilt on each rescan and published whole, so a reader never sees half an
/// update. Sorted: in-scope before out-of-scope, then working before idle before
/// dormant, then most-recently-active first.
#[derive(Debug, Clone)]
pub struct Roster {
    /// Where sessions were looked for.
    pub projects_dir: PathBuf,
    /// The repository this watch is about, when it is about one.
    pub repo: Option<String>,
    /// Every session found.
    pub sessions: Vec<LiveSession>,
    /// When the scan ran.
    pub scanned_at: SystemTime,
    /// Why the last scan found nothing, when it failed. Never fatal: a missing
    /// `~/.claude/projects` is a machine that has not run an agent yet, not a
    /// broken Polis.
    pub error: Option<String>,
}

impl Roster {
    /// An empty roster, for a watch whose first scan has not run.
    pub fn empty(projects_dir: &Path, repo: Option<String>) -> Self {
        Self {
            projects_dir: projects_dir.to_path_buf(),
            repo,
            sessions: Vec::new(),
            scanned_at: SystemTime::UNIX_EPOCH,
            error: None,
        }
    }

    /// Sessions **known** to be in the watched repository.
    ///
    /// A session whose `cwd` has not been read yet is deliberately not here: it
    /// is in [`Roster::resolving`] instead. A session being born is visible for
    /// about one rescan before its first record lands, and counting it as
    /// "working in this repository" during that window would put another
    /// checkout's agent under this repository's heading — briefly, plausibly,
    /// and wrongly. Measured against a real `claude -p` start: the transcript
    /// exists and is empty for well under a second.
    pub fn here(&self) -> impl Iterator<Item = &LiveSession> + '_ {
        self.sessions.iter().filter(|s| s.fit == Fit::Inside)
    }

    /// Sessions whose repository is not knowable yet — a session being born.
    ///
    /// Followed anyway when they started during this watch, so their first
    /// records are not lost while the question is being answered, but never
    /// counted as belonging anywhere.
    pub fn resolving(&self) -> impl Iterator<Item = &LiveSession> + '_ {
        self.sessions.iter().filter(|s| s.fit == Fit::Unknown)
    }

    /// Sessions in some other repository, which are reported and not drawn.
    pub fn elsewhere(&self) -> impl Iterator<Item = &LiveSession> + '_ {
        self.sessions.iter().filter(|s| s.fit == Fit::Outside)
    }

    /// Live sessions in the watched repository — the headline number.
    pub fn live_here(&self) -> usize {
        self.here().filter(|s| s.activity.is_live()).count()
    }

    /// Sessions in the watched repository that are working right now.
    pub fn working_here(&self) -> usize {
        self.here()
            .filter(|s| s.activity == Activity::Working)
            .count()
    }

    /// Live sessions in some other repository.
    pub fn live_elsewhere(&self) -> usize {
        self.elsewhere().filter(|s| s.activity.is_live()).count()
    }

    /// Live sessions whose repository is not knowable yet.
    pub fn live_resolving(&self) -> usize {
        self.resolving().filter(|s| s.activity.is_live()).count()
    }

    /// Sessions being followed right now.
    pub fn tailing(&self) -> usize {
        self.sessions.iter().filter(|s| s.tailing).count()
    }

    /// The sentence a status bar shows, and `polis doctor` prints.
    ///
    /// Says what is being seen *and* what is deliberately not, because "nothing
    /// is happening here" and "three agents are running, in another repository"
    /// are different situations and the failure this milestone exists to fix was
    /// not being able to tell them apart.
    pub fn headline(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        let live = self.live_here();
        let working = self.working_here();
        let mut text = match (live, working) {
            (0, _) => "no agent is running in this repository".to_owned(),
            (1, 1) => "1 agent working here".to_owned(),
            (1, _) => "1 agent here, idle".to_owned(),
            (n, 0) => format!("{n} agents here, all idle"),
            (n, w) if w == n => format!("{n} agents working here"),
            (n, w) => format!("{n} agents here, {w} working"),
        };
        let starting = self.live_resolving();
        if starting > 0 {
            let _ = write!(
                text,
                ", {starting} just starting{}",
                if starting == 1 { "" } else { " sessions" }
            );
        }
        let other = self.live_elsewhere();
        if other > 0 {
            let plural = if other == 1 {
                "another repository"
            } else {
                "other repositories"
            };
            let _ = write!(text, " ({other} more in {plural} — not drawn)");
        }
        text
    }
}

// ---------------------------------------------------------------------------
// The tailer
// ---------------------------------------------------------------------------

/// What one scan learnt about a session, kept between scans.
#[derive(Debug, Clone)]
struct Known {
    transcript: PathBuf,
    project_dir: PathBuf,
    bytes: u64,
    modified: Option<SystemTime>,
    started_while_watching: bool,
}

/// Follows every **live** session in one repository, discovering sessions that
/// start after the watch does (PRD §15 M3).
///
/// The scan is deliberately cheap: one `read_dir` per project directory and one
/// `metadata` per main transcript, every [`crate::transcript::RESCAN_EVERY`]
/// polls. Only sessions that pass the liveness gate get an open
/// [`SessionTailer`], and only those cost a `read_dir` of their sidecar
/// directory per poll.
#[derive(Debug)]
pub struct LiveTailer {
    projects_dir: PathBuf,
    scope: Scope,
    /// `cwd` learnt per **project directory**, not per session: the directory
    /// name is a function of the `cwd`, so every session under it shares one.
    /// `None` means "probed and nothing found yet", and is re-probed.
    project_cwd: BTreeMap<PathBuf, Option<String>>,
    known: BTreeMap<PathBuf, Known>,
    tails: BTreeMap<PathBuf, SessionTailer>,
    /// Sessions that existed before the watch started, so a session that appears
    /// later can be told from one that was already there.
    preexisting: BTreeSet<PathBuf>,
    roster: Roster,
    started: SystemTime,
    first_scan_done: bool,
}

impl LiveTailer {
    /// Opens a watch. Never fails on a missing projects directory: that is a
    /// machine on which no agent has ever run, and it must degrade with an
    /// explanation rather than stop the map (PRD §4, ADR-0011).
    pub fn open(projects_dir: &Path, scope: Scope) -> Self {
        let repo = scope.repo().map(ToOwned::to_owned);
        let mut tailer = Self {
            projects_dir: projects_dir.to_path_buf(),
            scope,
            project_cwd: BTreeMap::new(),
            known: BTreeMap::new(),
            tails: BTreeMap::new(),
            preexisting: BTreeSet::new(),
            roster: Roster::empty(projects_dir, repo),
            started: SystemTime::now(),
            first_scan_done: false,
        };
        tailer.rescan();
        tailer
    }

    /// Where sessions are discovered from.
    pub fn projects_dir(&self) -> &Path {
        &self.projects_dir
    }

    /// The current roster. Cheap: it is rebuilt on rescan and cloned here.
    pub fn roster(&self) -> Roster {
        self.roster.clone()
    }

    /// Why the channel is degraded, when it is.
    ///
    /// A watch that can see the directory but finds nothing in it is **not**
    /// degraded — an operator with no sessions yet is a normal state, and
    /// reporting it as a fault would make the one real fault invisible.
    pub fn degraded(&self) -> Option<&str> {
        self.roster.error.as_deref()
    }

    /// Rediscovers sessions, updates the roster, and opens or retires tails.
    pub fn rescan(&mut self) {
        let now = SystemTime::now();
        let projects = match discover_project_dirs(&self.projects_dir) {
            Ok(dirs) => dirs,
            Err(error) => {
                self.roster.error = Some(format!(
                    "cannot read {}: {error} — Polis sees no sessions until an agent has run \
                     on this machine at least once",
                    self.projects_dir.display()
                ));
                self.roster.scanned_at = now;
                self.roster.sessions.clear();
                self.known.clear();
                self.tails.clear();
                return;
            }
        };
        self.roster.error = None;

        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
        for project in projects {
            // The munged directory name is a function of the `cwd`, so one probe
            // serves every session under it. Re-probed while unknown, because a
            // directory created a moment ago has no record with a `cwd` in it
            // yet — which is exactly the session an operator just started.
            if !matches!(self.project_cwd.get(&project), Some(Some(_))) {
                let cwd = project_dir_cwd(&project);
                self.project_cwd.insert(project.clone(), cwd);
            }
            let Ok(sessions) = sessions_in(&project) else {
                continue;
            };
            for session_dir in sessions {
                seen.insert(session_dir.clone());
                self.observe(&project, &session_dir);
            }
        }
        // A session whose transcript was deleted stops being followed and stops
        // being reported, rather than becoming a permanent ghost in the roster.
        self.known.retain(|dir, _| seen.contains(dir));
        self.tails.retain(|dir, _| seen.contains(dir));
        if !self.first_scan_done {
            self.preexisting = seen;
            self.first_scan_done = true;
        }
        self.reconcile_tails(now);
        self.publish(now);
    }

    /// Records what one `metadata` call says about one session.
    fn observe(&mut self, project: &Path, session_dir: &Path) {
        let transcript = session_dir.with_extension("jsonl");
        let (bytes, modified) = match std::fs::metadata(&transcript) {
            Ok(meta) => (meta.len(), meta.modified().ok()),
            Err(_) => (0, None),
        };
        let started_while_watching =
            self.first_scan_done && !self.preexisting.contains(session_dir);
        // A session with a sidecar directory keeps growing there after the main
        // transcript has gone quiet — a main agent waiting on twelve subagents
        // writes nothing itself. Take the newest of the two, or a busy fan-out
        // looks idle.
        let modified = newer(modified, sidecar_modified(session_dir));
        self.known.insert(
            session_dir.to_path_buf(),
            Known {
                transcript,
                project_dir: project.to_path_buf(),
                bytes,
                modified,
                started_while_watching,
            },
        );
    }

    /// Opens tails for live in-scope sessions and retires the rest.
    fn reconcile_tails(&mut self, now: SystemTime) {
        let mut wanted: Vec<(PathBuf, SystemTime)> = Vec::new();
        for (dir, known) in &self.known {
            let cwd = self.cwd_of(&known.project_dir);
            let fit = self.scope.fit(cwd);
            let activity = activity_of(known.modified, now);
            let follow = match fit {
                Fit::Inside => activity.is_live(),
                // A session whose repository is not yet knowable is followed
                // only when it appeared during this watch: that is a session
                // being born, and its first records are the ones that say where
                // it is. Anything older with no readable `cwd` is history.
                Fit::Unknown => known.started_while_watching && activity.is_live(),
                Fit::Outside => false,
            };
            if follow {
                wanted.push((
                    dir.clone(),
                    known.modified.unwrap_or(SystemTime::UNIX_EPOCH),
                ));
            }
        }
        // Newest first, then capped: if the ceiling ever binds, it must drop the
        // stalest session rather than whichever one sorted last by path.
        wanted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        wanted.truncate(MAX_TAILED);
        let wanted: BTreeSet<PathBuf> = wanted.into_iter().map(|(dir, _)| dir).collect();

        self.tails.retain(|dir, _| wanted.contains(dir));
        for dir in wanted {
            if self.tails.contains_key(&dir) {
                continue;
            }
            let fresh = self
                .known
                .get(&dir)
                .is_some_and(|k| k.started_while_watching);
            let opened = if fresh {
                // Born during this watch: it starts empty, so reading it whole
                // is reading nothing.
                SessionTailer::open(&dir, StartAt::Beginning)
            } else {
                SessionTailer::resume(&dir, &backfill_offsets(&dir))
            };
            if let Ok(tailer) = opened {
                self.tails.insert(dir, tailer);
            }
        }
    }

    /// Rebuilds the published roster from `known` plus the open tails.
    fn publish(&mut self, now: SystemTime) {
        let mut sessions: Vec<LiveSession> = self
            .known
            .iter()
            .map(|(dir, known)| {
                let cwd = self.cwd_of(&known.project_dir).map(ToOwned::to_owned);
                let fit = self.scope.fit(cwd.as_deref());
                let activity = activity_of(known.modified, now);
                let tail = self.tails.get(dir);
                LiveSession {
                    session: SessionId::new(
                        dir.file_name().and_then(|n| n.to_str()).unwrap_or_default(),
                    ),
                    session_dir: dir.clone(),
                    transcript: known.transcript.clone(),
                    project_dir: known.project_dir.clone(),
                    cwd,
                    fit,
                    bytes: known.bytes,
                    modified: known.modified,
                    quiet_for: known.modified.and_then(|m| now.duration_since(m).ok()),
                    activity,
                    subagents: tail.map_or(0, |t| t.files().count().saturating_sub(1)),
                    tailing: tail.is_some(),
                    started_while_watching: known.started_while_watching,
                }
            })
            .collect();
        sessions.sort_by(|a, b| {
            let scope = |s: &LiveSession| match s.fit {
                Fit::Inside => 0u8,
                Fit::Unknown => 1,
                Fit::Outside => 2,
            };
            scope(a)
                .cmp(&scope(b))
                .then_with(|| a.activity.cmp(&b.activity))
                .then_with(|| b.modified.cmp(&a.modified))
                .then_with(|| a.session_dir.cmp(&b.session_dir))
        });
        self.roster.sessions = sessions;
        self.roster.scanned_at = now;
    }

    fn cwd_of(&self, project: &Path) -> Option<&str> {
        self.project_cwd.get(project)?.as_deref()
    }

    /// Reads every followed session's new bytes into `out`.
    ///
    /// Returns how many events were appended. Never rescans: the caller decides
    /// how often that costs, because a rescan is `read_dir` per project
    /// directory and a poll is a `metadata` per followed file.
    pub fn poll(&mut self, out: &mut Vec<Event>) -> usize {
        self.tails.values_mut().map(|t| t.poll(out)).sum()
    }

    /// How many sessions are being followed.
    pub fn len(&self) -> usize {
        self.tails.len()
    }

    /// True when nothing is being followed. Normal: it means no agent is
    /// working in this repository at the moment.
    pub fn is_empty(&self) -> bool {
        self.tails.is_empty()
    }

    /// Aggregated parse counters across every followed session.
    pub fn stats(&self) -> crate::transcript::ParseStats {
        let mut out = crate::transcript::ParseStats::default();
        for tail in self.tails.values() {
            out.merge(tail.stats());
        }
        out
    }

    /// When this watch started, so "appeared since" has a reference point.
    pub fn started(&self) -> SystemTime {
        self.started
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Classifies a modification time into an [`Activity`].
///
/// A file with no readable modification time is [`Activity::Dormant`]: unknown
/// must never read as live, or a machine whose clock or filesystem misbehaves
/// would have every session in its history opened at once.
fn activity_of(modified: Option<SystemTime>, now: SystemTime) -> Activity {
    let Some(modified) = modified else {
        return Activity::Dormant;
    };
    // A clock that stepped backwards, or a file written "in the future" by a
    // network share, is the most-live thing there is rather than the least.
    let quiet = now.duration_since(modified).unwrap_or(Duration::ZERO);
    if quiet <= WORKING_WINDOW {
        Activity::Working
    } else if quiet <= LIVE_WINDOW {
        Activity::Idle
    } else {
        Activity::Dormant
    }
}

/// The later of two optional times.
fn newer(a: Option<SystemTime>, b: Option<SystemTime>) -> Option<SystemTime> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (some, None) | (None, some) => some,
    }
}

/// The newest modification time anywhere under a session's sidecar directory.
///
/// One `metadata` on the directory itself, not a walk: on every filesystem Polis
/// runs on, creating or extending a file updates its directory's timestamp, and
/// a recursive walk per session per scan is exactly the cost this module exists
/// to avoid. Missing directory — the common case, since only a session with
/// subagents has one — is `None`.
fn sidecar_modified(session_dir: &Path) -> Option<SystemTime> {
    let direct = std::fs::metadata(session_dir).ok()?.modified().ok();
    let nested = std::fs::metadata(session_dir.join("subagents"))
        .ok()
        .and_then(|m| m.modified().ok());
    newer(direct, nested)
}

/// Byte offsets that make an already-running session resume from a bounded tail.
///
/// Each offset is the first record boundary at or after `len - `[`BACKFILL_BYTES`],
/// so no half-line is ever handed to the parser — the parser tolerates one, but
/// a junk line at the head of every attach is a schema-drift signal that is not
/// schema drift, and PRD §16 makes that counter mean something.
fn backfill_offsets(session_dir: &Path) -> BTreeMap<PathBuf, u64> {
    let Ok(files) = session_files(session_dir) else {
        return BTreeMap::new();
    };
    files
        .into_iter()
        .filter_map(|file| {
            let len = std::fs::metadata(&file.path).ok()?.len();
            if len <= BACKFILL_BYTES {
                return None; // Read whole; the default offset is already 0.
            }
            let offset =
                record_boundary_at_or_after(&file.path, len - BACKFILL_BYTES).unwrap_or(len);
            Some((file.path, offset))
        })
        .collect()
}

/// The offset just past the first `\n` at or after `from`, or `None`.
fn record_boundary_at_or_after(path: &Path, from: u64) -> Option<u64> {
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(from)).ok()?;
    // One record can be very large — a whole file read, a whole diff. Scanning a
    // megabyte for the boundary is bounded work and a record longer than that
    // simply means the backfill starts a little later than asked.
    let mut buf = vec![0u8; 1024 * 1024];
    let mut filled = 0usize;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    let found = buf[..filled].iter().position(|b| *b == b'\n')?;
    Some(from + found as u64 + 1)
}

/// `4m`, `2h`, `35s` — the coarse form a glance wants.
pub fn short_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 90 {
        format!("{secs}s")
    } else if secs < 90 * 60 {
        format!("{}m", secs / 60)
    } else if secs < 48 * 3600 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    /// A projects directory with one project dir whose sessions report `cwd`.
    fn project(root: &Path, key: &str) -> PathBuf {
        let dir = root.join(key);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_session(project: &Path, id: &str, cwd: &str, lines: usize) -> PathBuf {
        let path = project.join(format!("{id}.jsonl"));
        let mut file = std::fs::File::create(&path).unwrap();
        for i in 0..lines {
            writeln!(
                file,
                r#"{{"type":"user","uuid":"{id}-{i}","sessionId":"{id}","cwd":{},"timestamp":"2026-09-01T10:00:0{}Z","message":{{"role":"user","content":"hi"}}}}"#,
                serde_json::to_string(cwd).unwrap(),
                i % 10
            )
            .unwrap();
        }
        file.flush().unwrap();
        path
    }

    fn mapper_for(repo: &str) -> PathMapper {
        PathMapper::new(Path::new(repo)).unwrap()
    }

    /// The zero-setup promise: a session in the watched repository is found and
    /// followed with no hooks, no environment and no wrapper — and one in
    /// another repository is *reported*, not silently dropped.
    #[test]
    fn a_session_in_the_watched_repo_is_followed_and_one_elsewhere_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        std::fs::create_dir_all(&projects).unwrap();
        let here = project(&projects, "C--work-mine");
        let there = project(&projects, "C--work-other");
        write_session(&here, "aaaa", "C:/work/mine", 3);
        write_session(&there, "bbbb", "C:/work/other", 3);

        let tailer = LiveTailer::open(&projects, Scope::Repo(mapper_for("C:/work/mine")));
        let roster = tailer.roster();
        assert_eq!(roster.sessions.len(), 2, "{roster:?}");
        assert_eq!(roster.here().count(), 1);
        assert_eq!(roster.elsewhere().count(), 1);
        assert_eq!(
            roster.resolving().count(),
            0,
            "both sessions carry a cwd, so neither is still resolving"
        );
        let mine = roster.here().next().unwrap();
        assert_eq!(mine.session.as_str(), "aaaa");
        assert!(mine.tailing, "a live in-scope session must be followed");
        let theirs = roster.elsewhere().next().unwrap();
        assert_eq!(theirs.session.as_str(), "bbbb");
        assert!(
            !theirs.tailing,
            "another repository's session is reported, never drawn"
        );
        assert!(
            roster.headline().contains("another repository"),
            "the headline must say what is deliberately not shown: {}",
            roster.headline()
        );
    }

    /// Two degradations PRD §15 M3 names, and neither may stop the map: a
    /// projects directory with nothing in it, and a session whose repository has
    /// been deleted or renamed since it ran.
    ///
    /// The second is worth stating: [`Scope::fit`] never touches the filesystem,
    /// so a session pointing at a checkout that no longer exists is classified
    /// like any other — reported, out of scope, not followed. Resolving it
    /// against disk would make the roster's answer depend on whether some other
    /// directory still happens to be there.
    #[test]
    fn an_empty_corpus_and_a_deleted_repository_both_degrade_quietly() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        std::fs::create_dir_all(&projects).unwrap();

        // Nothing at all: a directory, and no sessions in it.
        let mut tailer = LiveTailer::open(&projects, Scope::Repo(mapper_for("C:/work/mine")));
        assert!(
            tailer.degraded().is_none(),
            "an empty corpus is not a fault"
        );
        assert!(tailer.roster().sessions.is_empty());
        assert_eq!(
            tailer.roster().headline(),
            "no agent is running in this repository"
        );

        // A session whose repository was deleted after it ran.
        let gone = project(&projects, "C--work-deleted");
        write_session(&gone, "ghost", "C:/work/deleted", 2);
        tailer.rescan();
        let roster = tailer.roster();
        assert_eq!(roster.sessions.len(), 1, "still reported");
        assert_eq!(roster.here().count(), 0);
        assert_eq!(roster.elsewhere().count(), 1);
        assert!(!roster.sessions[0].tailing);
        assert!(tailer.degraded().is_none(), "not a fault, just elsewhere");
    }

    /// The case an operator actually exercises: open the map, *then* start an
    /// agent. A session that did not exist at `open` must be discovered, read
    /// from byte zero, and marked as having started during the watch.
    #[test]
    fn a_session_that_starts_after_the_watch_is_discovered_and_read_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let here = project(&projects, "C--work-mine");
        write_session(&here, "old0", "C:/work/mine", 1);

        let mut tailer = LiveTailer::open(&projects, Scope::Repo(mapper_for("C:/work/mine")));
        assert_eq!(tailer.roster().sessions.len(), 1);
        let mut before = Vec::new();
        tailer.poll(&mut before);

        // The agent starts now.
        write_session(&here, "new1", "C:/work/mine", 4);
        tailer.rescan();
        let roster = tailer.roster();
        let fresh = roster
            .sessions
            .iter()
            .find(|s| s.session.as_str() == "new1")
            .expect("a session that starts after the watch must be discovered");
        assert!(fresh.started_while_watching);
        assert!(fresh.tailing);

        let mut out = Vec::new();
        tailer.poll(&mut out);
        assert!(
            out.len() >= 4,
            "a session born during the watch is read from zero, got {}",
            out.len()
        );
    }

    /// Several agents in one repository at once is the situation Polis exists
    /// for; all of them are followed and counted.
    #[test]
    fn several_sessions_in_one_repository_are_all_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let here = project(&projects, "C--work-mine");
        for id in ["a1", "b2", "c3"] {
            write_session(&here, id, "C:/work/mine", 2);
        }
        let tailer = LiveTailer::open(&projects, Scope::Repo(mapper_for("C:/work/mine")));
        let roster = tailer.roster();
        assert_eq!(roster.live_here(), 3);
        assert_eq!(roster.working_here(), 3);
        assert_eq!(roster.tailing(), 3);
        assert!(roster.headline().starts_with("3 agents working here"));
    }

    /// A vanished transcript stops being followed and stops being reported —
    /// never a ghost row that outlives the file.
    #[test]
    fn a_session_that_goes_away_leaves_the_roster() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let here = project(&projects, "C--work-mine");
        let path = write_session(&here, "gone", "C:/work/mine", 2);
        let mut tailer = LiveTailer::open(&projects, Scope::Repo(mapper_for("C:/work/mine")));
        assert_eq!(tailer.roster().sessions.len(), 1);
        std::fs::remove_file(&path).unwrap();
        tailer.rescan();
        assert!(tailer.roster().sessions.is_empty());
        assert_eq!(tailer.len(), 0);
    }

    /// No `~/.claude` at all: an explanation, an empty roster, and no error out
    /// of `open` — the map must still draw (PRD §4, ADR-0011).
    #[test]
    fn a_missing_projects_directory_explains_itself_and_never_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-dir");
        let tailer = LiveTailer::open(&missing, Scope::Everything);
        let roster = tailer.roster();
        assert!(roster.sessions.is_empty());
        let reason = tailer.degraded().expect("a missing directory says so");
        assert!(reason.contains("no-such-dir"), "{reason}");
        assert_eq!(roster.headline(), reason);
        // And it recovers when the directory appears, which is what happens the
        // first time an operator runs Claude Code.
        let mut tailer = tailer;
        std::fs::create_dir_all(&missing).unwrap();
        tailer.rescan();
        assert!(tailer.degraded().is_none());
    }

    /// `Scope::Repo` matches by path components, so a sibling directory whose
    /// name merely starts with the repository's is out of scope.
    #[test]
    fn scope_matches_components_not_string_prefixes() {
        let scope = Scope::Repo(mapper_for("C:/work/mine"));
        assert_eq!(scope.fit(Some("C:/work/mine")), Fit::Inside);
        assert_eq!(scope.fit(Some("C:/work/mine/src/deep")), Fit::Inside);
        assert_eq!(scope.fit(Some("C:/WORK/Mine")), Fit::Inside);
        assert_eq!(scope.fit(Some("C:/work/mine-backup")), Fit::Outside);
        assert_eq!(scope.fit(Some("C:/work/other")), Fit::Outside);
        assert_eq!(scope.fit(None), Fit::Unknown);
        // Everything admits any known cwd and still reports an unknown one.
        assert_eq!(Scope::Everything.fit(Some("C:/anywhere")), Fit::Inside);
        assert_eq!(Scope::Everything.fit(None), Fit::Unknown);
    }

    /// A session quiet for longer than the live window is not followed, which is
    /// what keeps a machine with a year of history at one `metadata` per session
    /// per scan rather than a thousand file handles.
    #[test]
    fn dormant_sessions_are_reported_but_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let here = project(&projects, "C--work-mine");
        let path = write_session(&here, "stale", "C:/work/mine", 2);
        // Backdate the transcript well past the live window.
        let old = SystemTime::now() - (LIVE_WINDOW + Duration::from_secs(600));
        set_modified(&path, old);

        let tailer = LiveTailer::open(&projects, Scope::Repo(mapper_for("C:/work/mine")));
        let roster = tailer.roster();
        assert_eq!(roster.sessions.len(), 1, "still reported");
        assert_eq!(roster.sessions[0].activity, Activity::Dormant);
        assert!(!roster.sessions[0].tailing);
        assert_eq!(roster.live_here(), 0);
        assert_eq!(roster.headline(), "no agent is running in this repository");
    }

    #[test]
    fn activity_is_a_function_of_silence_and_unknown_is_never_live() {
        let now = SystemTime::now();
        assert_eq!(activity_of(Some(now), now), Activity::Working);
        assert_eq!(
            activity_of(Some(now - WORKING_WINDOW - Duration::from_secs(1)), now),
            Activity::Idle
        );
        assert_eq!(
            activity_of(Some(now - LIVE_WINDOW - Duration::from_secs(1)), now),
            Activity::Dormant
        );
        assert_eq!(activity_of(None, now), Activity::Dormant);
        // A file stamped in the future is live, not dormant.
        assert_eq!(
            activity_of(Some(now + Duration::from_secs(3600)), now),
            Activity::Working
        );
        assert!(Activity::Working.is_live() && Activity::Idle.is_live());
        assert!(!Activity::Dormant.is_live());
    }

    /// The bounded catch-up: a large pre-existing transcript resumes at a
    /// **record boundary**, never mid-line.
    #[test]
    fn backfill_starts_at_a_record_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        let filler = "x".repeat(4096);
        for i in 0..900 {
            writeln!(file, r#"{{"i":{i},"pad":"{filler}"}}"#).unwrap();
        }
        file.flush().unwrap();
        drop(file);
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(len > BACKFILL_BYTES, "fixture must exceed the backfill");

        let offset = record_boundary_at_or_after(&path, len - BACKFILL_BYTES).unwrap();
        assert!(offset >= len - BACKFILL_BYTES && offset < len);
        let bytes = std::fs::read(&path).unwrap();
        let at = usize::try_from(offset).unwrap();
        assert_eq!(bytes[at - 1], b'\n', "cut just past a newline");
        assert_eq!(bytes[at], b'{', "and at the head of a record");
    }

    #[test]
    fn short_durations_read_at_a_glance() {
        assert_eq!(short_duration(Duration::from_secs(12)), "12s");
        assert_eq!(short_duration(Duration::from_secs(240)), "4m");
        assert_eq!(short_duration(Duration::from_secs(7200)), "2h");
        assert_eq!(short_duration(Duration::from_secs(400_000)), "4d");
    }

    fn set_modified(path: &Path, when: SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path);
        if let Ok(file) = file {
            let _ = file.set_modified(when);
        }
    }
}
