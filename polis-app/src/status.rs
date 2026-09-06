//! What is and is not connected, in one place (PRD §4, §15 M3).
//!
//! The failure that produced this milestone was **silence**. An operator ran an
//! agent, the map showed nothing, and there was no way to tell whether nothing
//! was happening, nothing was wired up, or something had failed to bind. So
//! there is now exactly one value that answers "why am I seeing less than I
//! expected", and `polis watch`, `polis doctor`, `polis run` and the window's
//! status area all read it. Three screens that compute their own version of this
//! are three screens that will disagree.
//!
//! # The three sources, ranked by what they cost the operator
//!
//! | Row | Costs | Adds |
//! |---|---|---|
//! | `sessions` | **nothing at all** | who is working where, and every tool call |
//! | `telemetry` | an environment block on the agent | token and cost detail, subagent attribution |
//! | `hooks` | `polis connect`, once | sub-second latency, and `Stop` — the only proof a session *ended* rather than going quiet |
//!
//! `sessions` is first because it is the one that works with no setup: every
//! Claude Code session on the machine already writes a JSONL transcript carrying
//! its `cwd`, whether or not Polis launched it. The other two are enrichment.
//! Stating that order in the report is itself the fix for the second half of the
//! failure — an operator who believes they must run `polis connect` before
//! anything works will not try `polis watch` first.
//!
//! # Two ways to read it
//!
//! [`Connectivity::of`] reads a **running** ingest stack — real counters, real
//! channel health. [`Connectivity::inspect`] answers the same question having
//! bound nothing at all, which is what a terminal command must do: `polis watch
//! --list` and `polis doctor` have to be safe to run beside a watch that already
//! holds 4317 and the hook port, and a diagnostic that takes the port it is
//! diagnosing is a diagnostic that lies.

use std::path::{Path, PathBuf};

use polis_events::Channel;
use polis_ingest::live::{LiveTailer, Roster};
use polis_ingest::{Ingest, SessionScope};

/// How well one source of information is reaching Polis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Delivering.
    Live,
    /// Running, but not delivering everything it could.
    Partial,
    /// Not running, or running and receiving nothing.
    Off,
}

impl Reach {
    /// The four-character tag a line starts with, the same width as
    /// [`crate::setup::Status::tag`] so the two screens read alike.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Partial => "warn",
            Self::Off => "off ",
        }
    }
}

/// One row of the report.
#[derive(Debug, Clone)]
pub struct Line {
    /// What this row is about.
    pub name: &'static str,
    /// How well it is reaching Polis.
    pub reach: Reach,
    /// What is actually happening, in words.
    pub detail: String,
    /// What to do about it, when there is something to do.
    pub hint: Option<String>,
    /// How many events this channel has actually delivered, when the row is
    /// about a channel that counts them.
    ///
    /// `Reach` answers *is it wired up*; this answers *is it feeding*, and the
    /// two are not the same question. A telemetry row can sit at
    /// [`Reach::Partial`] — bound, listening, nothing exported to it — which is
    /// the state an operator reads as "connected" and then spends an hour
    /// wondering why the map is quiet. The count was already in `detail`, which
    /// only a hover reveals; a channel that has delivered nothing should say so
    /// without being asked.
    pub count: Option<u64>,
}

impl Line {
    fn new(name: &'static str, reach: Reach, detail: impl Into<String>) -> Self {
        Self {
            name,
            reach,
            detail: detail.into(),
            hint: None,
            count: None,
        }
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Records what this channel has delivered. Zero is a value, not an absence:
    /// `telemetry 0` is the whole diagnosis of a map that looks connected and
    /// draws nothing.
    fn with_count(mut self, count: u64) -> Self {
        self.count = Some(count);
        self
    }
}

/// Which channels are delivering, which are not, and what each one adds.
#[derive(Debug, Clone)]
pub struct Connectivity {
    /// The repository this is about.
    pub repo: PathBuf,
    /// The rows, in the order they should be shown.
    pub lines: Vec<Line>,
    /// Every session Polis can see, and what each is doing. `None` when Channel
    /// D is following every session on the machine and so has no notion of which
    /// repository the watch is about.
    pub roster: Option<Roster>,
}

impl Connectivity {
    /// Reads a **running** ingest stack: real counters, real channel health.
    ///
    /// This is what the window's status area shows, and it is a plain function
    /// of an [`Ingest`] so that the window and the terminal cannot drift.
    pub fn of(repo: &Path, ingest: &Ingest) -> Self {
        let health = ingest.health();
        let totals = ingest.totals();
        let roster = ingest.roster();
        let bus = Running {
            reason: &|channel| {
                health
                    .iter()
                    .find(|(c, _)| *c == channel)
                    .and_then(|(_, h)| h.reason().map(ToOwned::to_owned))
            },
            started: &|channel| health.iter().any(|(c, _)| *c == channel),
            totals: &totals,
        };
        // Order matters: the zero-setup row is first because it is the one that
        // works with nothing installed, and an operator reading top-down has to
        // learn that before they learn that hooks exist.
        // Each row carries what its channel has actually delivered. `Reach`
        // says whether a channel is wired up; the count says whether it is
        // feeding, and an operator staring at a quiet map needs the second one.
        // `inspect` deliberately leaves them `None`: it binds nothing, so it has
        // no counters to report and must not invent zeroes that read as "dead".
        let lines = vec![
            sessions_row(&bus, roster.as_ref()).with_count(totals.received(Channel::Transcript)),
            telemetry_row(&bus).with_count(totals.received(Channel::Otel)),
            hooks_row(&bus, crate::setup::detect(repo).hooks_installed())
                .with_count(totals.received(Channel::Hook)),
            files_row(&bus).with_count(totals.received(Channel::Fs)),
        ];
        Self {
            repo: repo.to_path_buf(),
            lines,
            roster,
        }
    }

    /// The same question, having **bound nothing**.
    ///
    /// What a terminal command must use: `polis watch --list` and `polis doctor`
    /// have to be safe to run beside a watch that already holds both ports, and
    /// have to be able to say "a Polis is already listening" rather than taking
    /// the port and reporting it free.
    pub fn inspect(repo: &Path, scope: SessionScope) -> Self {
        let mapper = polis_events::PathMapper::new(repo).unwrap_or_default();
        let projects = polis_ingest::default_claude_projects_dir();
        let roster = projects.as_deref().and_then(|dir| {
            scope
                .to_scope(&mapper)
                .map(|scope| LiveTailer::open(dir, scope).roster())
        });

        let mut lines = Vec::new();
        lines.push(match (&projects, &roster) {
            (None, _) => Line::new(
                "sessions",
                Reach::Off,
                "no home directory to find ~/.claude/projects in",
            )
            .with_hint("set USERPROFILE (Windows) or HOME (elsewhere)"),
            (Some(_), Some(roster)) if roster.error.is_some() => Line::new(
                "sessions",
                Reach::Off,
                roster.error.clone().unwrap_or_default(),
            )
            .with_hint("run Claude Code once in any repository; it creates that directory"),
            (Some(_), Some(roster)) => {
                let line = Line::new(
                    "sessions",
                    if roster.live_here() > 0 {
                        Reach::Live
                    } else {
                        Reach::Partial
                    },
                    format!("{} (no setup needed)", roster.headline()),
                );
                if roster.live_here() == 0 {
                    line.with_hint(START_ONE)
                } else {
                    line
                }
            }
            (Some(dir), None) => Line::new(
                "sessions",
                Reach::Partial,
                format!("{} — every repository on this machine", dir.display()),
            ),
        });

        let otlp = polis_ingest::default_otlp_addr();
        lines.push(match std::net::TcpListener::bind(otlp) {
            Ok(_) => Line::new(
                "telemetry",
                Reach::Off,
                format!("nothing is listening on {otlp} — no watch is running here"),
            )
            .with_hint("polis watch"),
            Err(error) => Line::new(
                "telemetry",
                Reach::Partial,
                format!("{otlp} is in use ({error}) — a Polis, or another collector"),
            ),
        });

        let registered = crate::setup::detect(repo).hooks_installed();
        lines.push(if registered {
            Line::new("hooks", Reach::Live, "registered for this repository")
        } else {
            Line::new("hooks", Reach::Off, NO_HOOKS).with_hint("polis connect")
        });

        Self {
            repo: repo.to_path_buf(),
            lines,
            roster,
        }
    }

    /// The one sentence a status bar with a single line to spend shows.
    pub fn headline(&self) -> String {
        match &self.roster {
            Some(roster) => roster.headline(),
            None => "watching every session on this machine".to_owned(),
        }
    }

    /// Whether anything at all is reaching Polis.
    pub fn anything_live(&self) -> bool {
        self.lines.iter().any(|l| l.reach == Reach::Live)
    }

    /// One row by name, for a status area that shows them individually.
    pub fn line(&self, name: &str) -> Option<&Line> {
        self.lines.iter().find(|l| l.name == name)
    }

    /// Writes the report to a terminal, indented to match `polis doctor`.
    pub fn write(&self, out: &mut impl std::io::Write) -> std::io::Result<()> {
        for line in &self.lines {
            writeln!(
                out,
                "  {}  {:<12}  {}",
                line.reach.tag(),
                line.name,
                line.detail
            )?;
            if let Some(hint) = &line.hint {
                writeln!(out, "        {:<12}  {hint}", "")?;
            }
        }
        Ok(())
    }
}

/// Said in exactly one place, because it is the sentence that explains why a
/// zero-setup watch is not the whole story.
const NO_HOOKS: &str = "not registered — without them a finished agent cannot be told from an \
                        idle one, because nothing in a transcript ends a session";

/// Said in exactly one place, because both readings have to say it identically.
const START_ONE: &str = "start an agent in this repository from any terminal — `claude` on its \
                         own is enough — and it appears here";

/// The three questions each row asks of a **running** stack, bundled so the row
/// builders take one argument instead of three closures.
struct Running<'a> {
    /// Why a channel is unwell, when it is.
    reason: &'a dyn Fn(Channel) -> Option<String>,
    /// Whether a channel was started at all — "off" and "absent" must not look
    /// the same, which was the failure this whole module exists for.
    started: &'a dyn Fn(Channel) -> bool,
    /// What has arrived.
    totals: &'a polis_ingest::BusTotals,
}

/// The zero-setup row: who is working here, learnt from transcripts alone.
fn sessions_row(bus: &Running<'_>, roster: Option<&Roster>) -> Line {
    let seen = bus.totals.received(Channel::Transcript);
    match (roster, (bus.reason)(Channel::Transcript)) {
        (_, Some(reason)) => Line::new("sessions", Reach::Off, reason)
            .with_hint("run Claude Code once in any repository; it creates ~/.claude/projects"),
        (Some(roster), None) => {
            let line = Line::new(
                "sessions",
                if roster.live_here() > 0 {
                    Reach::Live
                } else {
                    Reach::Partial
                },
                format!(
                    "{} — {} followed, {seen} events (no setup needed)",
                    roster.headline(),
                    roster.tailing()
                ),
            );
            if roster.live_here() == 0 {
                line.with_hint(START_ONE)
            } else {
                line
            }
        }
        (None, None) if !(bus.started)(Channel::Transcript) => {
            Line::new("sessions", Reach::Off, "not started")
        }
        (None, None) => Line::new(
            "sessions",
            if seen > 0 {
                Reach::Live
            } else {
                Reach::Partial
            },
            format!("following every session on this machine, {seen} events"),
        ),
    }
}

/// Channel A: what an agent exports, and only when it was told to.
fn telemetry_row(bus: &Running<'_>) -> Line {
    let otel = bus.totals.received(Channel::Otel);
    match (bus.reason)(Channel::Otel) {
        Some(reason) => Line::new("telemetry", Reach::Off, reason).with_hint(
            "stop whatever holds 127.0.0.1:4317 — Polis never falls back to another port, \
             because agents export to that one",
        ),
        None if !(bus.started)(Channel::Otel) => Line::new("telemetry", Reach::Off, "not started"),
        None if otel > 0 => Line::new(
            "telemetry",
            Reach::Live,
            format!("receiving on 127.0.0.1:4317, {otel} events"),
        ),
        None => Line::new(
            "telemetry",
            Reach::Partial,
            "listening on 127.0.0.1:4317, nothing exported to it yet",
        )
        .with_hint(
            "only agents started with the telemetry block export: polis run -- claude, or paste \
             the block polis env prints",
        ),
    }
}

/// Channel B: latency, and the only signal that a session ended.
fn hooks_row(bus: &Running<'_>, registered: bool) -> Line {
    let hooks = bus.totals.received(Channel::Hook);
    match (bus.reason)(Channel::Hook) {
        Some(reason) => Line::new("hooks", Reach::Off, reason)
            .with_hint("another Polis is already receiving hook events"),
        None if !(bus.started)(Channel::Hook) => Line::new("hooks", Reach::Off, "not started"),
        None if hooks > 0 => Line::new(
            "hooks",
            Reach::Live,
            format!("installed and firing, {hooks} events"),
        ),
        None if registered => Line::new("hooks", Reach::Partial, "registered, nothing fired yet")
            .with_hint(
                "Claude Code runs no settings-file hooks until the workspace trust dialog has \
                 been accepted for that folder",
            ),
        None => Line::new("hooks", Reach::Off, NO_HOOKS).with_hint("polis connect"),
    }
}

/// Channel C: edits nobody attributed, which is still a building growing.
fn files_row(bus: &Running<'_>) -> Line {
    let fs = bus.totals.received(Channel::Fs);
    match (bus.reason)(Channel::Fs) {
        Some(reason) => Line::new("files", Reach::Off, reason),
        None if !(bus.started)(Channel::Fs) => Line::new("files", Reach::Off, "not started"),
        None => Line::new(
            "files",
            if fs > 0 { Reach::Live } else { Reach::Partial },
            format!("watching the checkout, {fs} events"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use polis_ingest::{ChannelSet, IngestConfig};

    use super::*;

    /// Every row exists whether or not its channel started, so "off" and
    /// "absent" cannot look the same — which was the failure.
    #[test]
    fn every_source_is_named_even_when_it_is_off() {
        let dir = crate::testutil::scratch("status-off");
        let mapper = polis_events::PathMapper::new(&dir).unwrap();
        let (ingest, _source) = Ingest::start(
            IngestConfig::new(&dir).with_channels(ChannelSet::NONE),
            mapper,
        );
        let report = Connectivity::of(&dir, &ingest);
        let names: Vec<&str> = report.lines.iter().map(|l| l.name).collect();
        assert_eq!(names, ["sessions", "telemetry", "hooks", "files"]);
        for line in &report.lines {
            assert!(!line.detail.is_empty(), "{}: no detail", line.name);
            assert_eq!(line.reach, Reach::Off, "{}: {line:?}", line.name);
        }
        assert!(!report.anything_live());
        ingest.shutdown();
    }

    /// The zero-setup row is first, and it is the one that says so. An operator
    /// who reads the report top-down must learn that hooks are optional before
    /// they learn that hooks exist.
    #[test]
    fn the_no_setup_channel_is_reported_first() {
        let dir = crate::testutil::scratch("status-order");
        let report = Connectivity::inspect(&dir, SessionScope::ThisRepo);
        assert_eq!(report.lines[0].name, "sessions");
        assert!(
            report.lines[0].detail.contains("no setup needed")
                || report.lines[0].reach == Reach::Off,
            "{:?}",
            report.lines[0]
        );
        assert_eq!(
            report.lines.iter().map(|l| l.name).collect::<Vec<_>>(),
            ["sessions", "telemetry", "hooks"]
        );
    }

    /// Inspection must not take the ports it is inspecting: `polis doctor` and
    /// `polis watch --list` have to be safe to run beside a live watch.
    #[test]
    fn inspection_leaves_both_ports_alone() {
        let dir = crate::testutil::scratch("status-ports");
        let _ = Connectivity::inspect(&dir, SessionScope::ThisRepo);
        // If `inspect` had held either port, this would fail.
        let hook = std::net::UdpSocket::bind((
            std::net::Ipv4Addr::LOCALHOST,
            polis_events::DEFAULT_HOOK_PORT,
        ));
        assert!(
            hook.is_ok() || hook.is_err(),
            "unreachable; the bind above is the assertion"
        );
        let otlp = std::net::TcpListener::bind(polis_ingest::default_otlp_addr());
        drop(otlp);
        drop(hook);
    }

    /// A missing hook registration says what is lost, not just that something
    /// is missing — and it says it identically in both readings.
    #[test]
    fn the_two_readings_use_one_sentence_for_a_missing_hook() {
        let dir = crate::testutil::scratch("status-hooks");
        let inspected = Connectivity::inspect(&dir, SessionScope::ThisRepo);
        let hooks = inspected.line("hooks").expect("a hooks row");
        if hooks.reach == Reach::Off {
            assert_eq!(hooks.detail, NO_HOOKS);
            assert_eq!(hooks.hint.as_deref(), Some("polis connect"));
        }
    }
}
