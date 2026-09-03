//! `polis watch` — every agent working in a repository, with zero setup
//! (PRD §15 M3).
//!
//! ```text
//! polis watch
//! ```
//!
//! Point it at a repository and it shows every Claude Code session working in
//! that repository **right now** — including the ones the operator started
//! themselves, in other terminals, that Polis never launched. That last clause
//! is the whole point, and it is what the design before this could not do:
//! `polis run` could only ever show the one agent it had spawned, and everything
//! else needed hooks installed or a telemetry block exported before a single
//! event existed.
//!
//! # Why it needs nothing
//!
//! Every Claude Code session on this machine appends to a JSONL transcript under
//! `~/.claude/projects`, and every threaded record in it carries the `cwd` it was
//! written from. Presence, activity and every tool call for every agent in a
//! repository are therefore already on disk.
//! [`polis_ingest::live::LiveTailer`] discovers them, filters them to this
//! checkout, follows the live ones and publishes a
//! [`polis_ingest::live::Roster`]. No hooks, no environment variables, no
//! wrapper process, nothing to install.
//!
//! Telemetry and hooks then add detail on top — token counts, subagent
//! attribution, sub-second latency, and `Stop`, which is the only signal that
//! proves a session *ended* rather than merely going quiet. They are an
//! enrichment, and both this command and the window say plainly which of them
//! are connected ([`crate::status::Connectivity`]), because the failure that
//! produced this milestone was silence: an operator watching an empty map with
//! no way to tell whether nothing was happening or nothing was wired up.
//!
//! # What it does, in order
//!
//! 1. Says what it can and cannot see, **before** the window steals the screen:
//!    the roster, and which of the four channels will be delivering.
//! 2. Opens the window in [`crate::Mode::Live`], which starts the four channels
//!    and drives the world from them.
//!
//! `--list` stops after step 1 and binds nothing at all, which makes it safe to
//! run beside a watch that is already up — and makes it the thing to reach for
//! when the question is "is anything running?" rather than "show me".

use std::io::{self, Write};
use std::path::Path;

use anyhow::Context as _;
use polis_ingest::live::Roster;

use crate::cli::{Cli, WatchArgs};
use crate::config::Config;
use crate::status::Connectivity;

/// `polis watch`.
pub fn watch(cli: &Cli, args: &WatchArgs) -> anyhow::Result<i32> {
    let repo = cli.repo_root().context("resolving the repository root")?;
    let options = args.options(repo.clone());
    let report = Connectivity::inspect(&repo, options.ingest.sessions);

    if args.list {
        return list(&repo, &report);
    }

    banner(&repo, args, &report)?;
    let config = Config {
        repo_root: repo,
        ..Config::default()
    };
    crate::launch(
        config,
        crate::Mode::Live {
            options: Box::new(options),
        },
    )?;
    Ok(0)
}

/// `polis watch --list`: what is running, as text, binding nothing.
fn list(repo: &Path, report: &Connectivity) -> anyhow::Result<i32> {
    let mut out = io::stdout().lock();
    writeln!(out)?;
    writeln!(out, "  polis watch — {}", repo.display())?;
    writeln!(out)?;
    report.write(&mut out)?;
    writeln!(out)?;
    if let Some(roster) = &report.roster {
        write_roster(&mut out, roster)?;
        writeln!(out)?;
    }
    out.flush()?;
    Ok(0)
}

/// The session table, in-scope sessions first.
///
/// Sessions in another repository are listed and labelled rather than dropped:
/// "nothing is happening here" and "three agents are running, in a checkout this
/// window does not map" are different situations, and the operator has to be
/// able to tell them apart. One Polis window maps one repository (PRD §2), so
/// the remedy is another window, and this says so.
pub fn write_roster(out: &mut impl Write, roster: &Roster) -> io::Result<()> {
    let here: Vec<_> = roster.here().filter(|s| s.activity.is_live()).collect();
    let elsewhere: Vec<_> = roster
        .elsewhere()
        .filter(|s| s.activity.is_live())
        .collect();

    if here.is_empty() {
        writeln!(
            out,
            "  No agent is working in this repository. Start one in any terminal —\n  \
             `claude` on its own is enough — and it appears here, with no setup at all."
        )?;
    } else {
        writeln!(out, "  In this repository:")?;
        for session in &here {
            writeln!(out, "    {}", session.line())?;
        }
    }
    let starting: Vec<_> = roster
        .resolving()
        .filter(|s| s.activity.is_live())
        .collect();
    if !starting.is_empty() {
        writeln!(out)?;
        writeln!(
            out,
            "  Just starting — followed already, but the repository is not in their\n  \
             records yet:"
        )?;
        for session in starting.iter().take(ELSEWHERE_SHOWN) {
            writeln!(out, "    {}", session.line())?;
        }
    }
    if !elsewhere.is_empty() {
        writeln!(out)?;
        writeln!(
            out,
            "  Elsewhere on this machine — reported, not drawn, because one Polis window\n  \
             maps one repository:"
        )?;
        for session in elsewhere.iter().take(ELSEWHERE_SHOWN) {
            writeln!(out, "    {}", session.line())?;
        }
        if elsewhere.len() > ELSEWHERE_SHOWN {
            writeln!(out, "    … and {} more", elsewhere.len() - ELSEWHERE_SHOWN)?;
        }
        writeln!(out)?;
        writeln!(
            out,
            "  To watch one of those:  polis -C <that repository> watch"
        )?;
    }
    Ok(())
}

/// How many other-repository sessions are listed before the count takes over.
const ELSEWHERE_SHOWN: usize = 8;

/// The lines printed before the window opens.
///
/// Printed *before*, deliberately: the window takes the screen and blocks until
/// it is closed, so this is the operator's one chance to see what was found and
/// what was not.
fn banner(repo: &Path, args: &WatchArgs, report: &Connectivity) -> anyhow::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out)?;
    writeln!(
        out,
        "  P O L I S   ·   every agent working in this repository"
    )?;
    writeln!(out)?;
    writeln!(out, "    repo        {}", repo.display())?;
    if args.machine {
        writeln!(
            out,
            "    scope       every repository on this machine (--machine)"
        )?;
    }
    writeln!(out)?;
    report.write(&mut out)?;
    writeln!(out)?;
    if let Some(roster) = &report.roster {
        write_roster(&mut out, roster)?;
        writeln!(out)?;
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use polis_ingest::live::{Activity, Fit, LiveSession};
    use polis_ingest::SessionScope;

    use super::*;

    fn session(id: &str, cwd: &str, fit: Fit) -> LiveSession {
        LiveSession {
            session: polis_events::SessionId::new(id),
            session_dir: PathBuf::from(format!("/p/{id}")),
            transcript: PathBuf::from(format!("/p/{id}.jsonl")),
            project_dir: PathBuf::from("/p"),
            cwd: Some(cwd.to_owned()),
            fit,
            bytes: 100,
            modified: None,
            quiet_for: None,
            activity: Activity::Working,
            subagents: 0,
            tailing: fit == Fit::Inside,
            started_while_watching: false,
        }
    }

    fn roster(sessions: Vec<LiveSession>) -> Roster {
        let mut roster = Roster::empty(Path::new("/p"), Some("/repo".to_owned()));
        roster.sessions = sessions;
        roster
    }

    /// A roster with nothing in it says how to make it non-empty, rather than
    /// printing an empty table — and it names the zero-setup route.
    #[test]
    fn an_empty_roster_says_how_to_make_it_non_empty() {
        let mut out = Vec::new();
        write_roster(&mut out, &roster(Vec::new())).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("No agent is working in this repository"),
            "{text}"
        );
        assert!(text.contains("`claude` on its own is enough"), "{text}");
        assert!(text.contains("no setup at all"), "{text}");
    }

    /// A session in another repository is reported as such, never silently
    /// ignored, and the operator is told what to do about it.
    #[test]
    fn a_session_in_another_repository_is_reported_with_its_remedy() {
        let mut out = Vec::new();
        write_roster(
            &mut out,
            &roster(vec![
                session("mine", "/repo", Fit::Inside),
                session("theirs", "/somewhere/else", Fit::Outside),
            ]),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("In this repository:"), "{text}");
        assert!(text.contains("mine"), "{text}");
        assert!(text.contains("Elsewhere on this machine"), "{text}");
        assert!(text.contains("/somewhere/else"), "{text}");
        assert!(text.contains("polis -C <that repository> watch"), "{text}");
    }

    /// A machine with a hundred other sessions must not push the one that
    /// matters off the screen.
    #[test]
    fn a_long_elsewhere_list_is_summarised() {
        let mut sessions = vec![session("mine", "/repo", Fit::Inside)];
        for i in 0..20 {
            sessions.push(session(&format!("other{i}"), "/elsewhere", Fit::Outside));
        }
        let mut out = Vec::new();
        write_roster(&mut out, &roster(sessions)).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("… and 12 more"), "{text}");
    }

    /// `--list` is pure inspection: it reads the same report the window shows
    /// and binds nothing, so it is safe beside a running watch.
    #[test]
    fn list_prints_the_same_report_the_window_will_show() {
        let dir = crate::testutil::scratch("watch-list");
        let report = Connectivity::inspect(&dir, SessionScope::ThisRepo);
        let mut out = Vec::new();
        report.write(&mut out).unwrap();
        if let Some(roster) = &report.roster {
            write_roster(&mut out, roster).unwrap();
        }
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("sessions"), "{text}");
        assert!(text.contains("telemetry"), "{text}");
        assert!(text.contains("hooks"), "{text}");
    }
}
