//! `polis run` — the one-command connect (PRD §4.1, §15).
//!
//! Before this existed, watching your own agent required knowing three separate
//! things: that twelve environment variables have to be exported into the shell
//! that starts `claude`, that a receiver has to be listening on 4317 before the
//! agent starts or the first minute is lost, and that the window is a separate
//! command you run yourself. That is a configuration exercise, not a front door.
//!
//! ```text
//! polis run -- claude
//! ```
//!
//! does all three: it starts the receiver, opens the map, and launches the agent
//! as a **child process** with the telemetry environment already set on it — the
//! exact block `polis env` prints, applied to one process instead of to the
//! operator's shell.
//!
//! # Why the agent is in the foreground and the window is the child
//!
//! It could be the other way round — `winit` owns the main thread, so the window
//! would have to be here and the agent on a thread. Three things decide it:
//!
//! 1. **The agent is a full-screen terminal application.** It needs this
//!    terminal's stdin, stdout and stderr, unredirected, with the real console
//!    handle behind them. Inheriting them from a process that is doing nothing
//!    else is exactly that; anything else is a pty emulation Polis has no reason
//!    to write.
//! 2. **The exit code is the agent's.** `polis run -- claude -p "…"` in a script
//!    has to behave like `claude -p "…"`, including when it fails.
//! 3. **The window prints to stderr** — the adapter line, wgpu's own warnings —
//!    and that would land in the middle of the agent's redrawing UI. As a child
//!    with null stdio it cannot.
//!
//! So the window is spawned as `polis map`, silenced, and outlives the command:
//! closing the agent does not take the map away from you mid-thought.
//!
//! # What is live today, stated plainly
//!
//! The receiver is real and the counts at the end are what it actually saw. The
//! map window, until live ingest is wired into it, draws the city and the
//! session you point it at rather than the run in progress — so `polis run`
//! finishes by printing the `polis replay` line for the session that just
//! happened. That is one command away from watching it move, and it is honest
//! about which is which.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use anyhow::Context as _;
use polis_events::{PathMapper, RecordingClock, RecordingHeader};
use polis_ingest::env as hookenv;
use polis_ingest::{Ingest, IngestConfig};

use crate::cli::{Cli, RunArgs};
use crate::setup;

/// The agent `polis run` launches when the operator names none.
pub const DEFAULT_AGENT: &str = "claude";

/// Marks the map window Polis spawned for itself.
///
/// Two uses, and both are about a process with nobody at it: it is what
/// [`setup::should_pause`] checks so that window can never sit waiting for a
/// keypress, and it is how an operator looking at two `polis` processes in a
/// task manager can tell which one they started.
pub const WINDOW_CHILD_ENV: &str = "POLIS_WINDOW_CHILD";

/// Runs an agent with Polis watching. Returns the agent's exit code.
///
/// Every step prints one line, because the operator is about to lose the screen
/// to a full-screen agent UI and this is their only chance to see what was set
/// up for them.
pub fn run(cli: &Cli, args: &RunArgs) -> anyhow::Result<i32> {
    let repo = cli.repo_root().context("resolving the repository root")?;
    let (program, passthrough) = resolve_agent(args)?;
    let env = hookenv::agent_env(&format!("http://{}", args.otlp_addr));

    // The receiver first, and before the agent: an exporter that finds nothing
    // listening drops its first batch, and the first batch is the session start.
    let mapper = PathMapper::new(&repo).unwrap_or_default();
    let mut config = IngestConfig::new(&repo);
    config.otlp_addr = args.otlp_addr;
    let (ingest, source) = Ingest::start(config, mapper);

    // The drain has to run: an unread bus fills and then drops, and a dropped
    // event is a lie in the counters at the end. It also writes the recording,
    // when one was asked for.
    let record = args.record.clone();
    let clock = RecordingClock::start_now();
    let drain = std::thread::Builder::new()
        .name("polis-run-drain".to_owned())
        .spawn(move || drain_bus(&source, record.as_deref(), clock))
        .context("starting the event drain")?;

    let window = if args.no_window {
        None
    } else {
        spawn_window(&repo).ok()
    };
    banner(&repo, &program, args, &ingest, env.len(), window.is_some())?;

    // Before the agent, not after: the transcript it writes is identified by
    // being new, and "new" needs a picture of the world from before it ran.
    let before = transcripts_now();
    let started = Instant::now();
    let mut command = setup::command_for(&program, &passthrough);
    command
        .current_dir(&repo)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (key, value) in &env {
        command.env(key, value);
    }
    let status = command
        .status()
        .with_context(|| format!("running {}", program.display()))?;
    let elapsed = started.elapsed();

    let totals = ingest.totals();
    let dropped = ingest.stats().dropped_total();
    ingest.shutdown();
    let recorded = drain.join().unwrap_or(Ok(0)).unwrap_or(0);

    let code = status.code().unwrap_or(i32::from(!status.success()));
    summary(
        &repo,
        args,
        &Outcome {
            code,
            elapsed,
            before: &before,
            totals: &totals,
            dropped,
            recorded,
            window_open: window.is_some(),
        },
    )?;
    Ok(code)
}

/// What the run cost and produced, for [`summary`].
struct Outcome<'a> {
    code: i32,
    elapsed: std::time::Duration,
    /// Every transcript that existed before the agent started, so the one it
    /// wrote can be told from one another agent wrote in the same checkout
    /// while it ran.
    before: &'a BTreeSet<PathBuf>,
    totals: &'a polis_ingest::BusTotals,
    dropped: u64,
    recorded: u64,
    window_open: bool,
}

/// The lines printed before the terminal belongs to the agent.
///
/// The operator is about to lose the screen to a full-screen UI, so this is
/// their one chance to see what was set up on their behalf — and, when a channel
/// failed to start, to see that too rather than wondering later why the map is
/// empty.
fn banner(
    repo: &Path,
    program: &Path,
    args: &RunArgs,
    ingest: &Ingest,
    variables: usize,
    window: bool,
) -> anyhow::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out)?;
    writeln!(out, "  P O L I S   ·   your agent, with the map watching")?;
    writeln!(out)?;
    writeln!(out, "    repo        {}", repo.display())?;
    writeln!(out, "    agent       {}", program.display())?;
    writeln!(
        out,
        "    receiver    {} (telemetry), 127.0.0.1:{} (hooks)",
        args.otlp_addr,
        polis_events::DEFAULT_HOOK_PORT
    )?;
    writeln!(
        out,
        "    telemetry   {variables} variables set on that one process — not on your shell"
    )?;
    writeln!(
        out,
        "    window      {}",
        if window {
            "opening"
        } else if args.no_window {
            "not opened (--no-window)"
        } else {
            "could not be opened — the agent still runs"
        }
    )?;
    if let Some(path) = &args.record {
        writeln!(out, "    recording   {}", path.display())?;
    }
    for (channel, health) in ingest.health() {
        if let Some(reason) = health.reason() {
            writeln!(out, "    warning     {channel}: {reason}")?;
        }
    }
    if let Some(conflict) = ingest.hook_conflict() {
        writeln!(out, "    warning     {conflict}")?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "  Everything below is your agent. Ctrl-C, /exit and the exit code all\n  \
         behave exactly as they would without Polis."
    )?;
    writeln!(out, "  {}", "─".repeat(68))?;
    out.flush()?;
    Ok(())
}

/// The lines printed once the agent has exited.
fn summary(repo: &Path, args: &RunArgs, outcome: &Outcome<'_>) -> anyhow::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "  {}", "─".repeat(68))?;
    writeln!(
        out,
        "  Agent exited with code {} after {}.",
        outcome.code,
        crate::format::duration(outcome.elapsed)
    )?;
    writeln!(
        out,
        "  Polis received {} events — telemetry {}, hooks {}, files {}, transcript {}{}",
        outcome.totals.received_total(),
        outcome.totals.received(polis_events::Channel::Otel),
        outcome.totals.received(polis_events::Channel::Hook),
        outcome.totals.received(polis_events::Channel::Fs),
        outcome.totals.received(polis_events::Channel::Transcript),
        if outcome.dropped == 0 {
            String::new()
        } else {
            format!(" ({} dropped)", outcome.dropped)
        }
    )?;
    if outcome.totals.received_total() == 0 {
        writeln!(out)?;
        writeln!(
            out,
            "  Nothing arrived. That is almost always one of two things:\n    \
             the agent is not Claude Code, so it exports no telemetry; or\n    \
             hooks are not installed — run  polis connect  once.\n  \
             `polis doctor` checks both."
        )?;
    }
    if let Some(path) = &args.record {
        writeln!(
            out,
            "  Recorded {} events to {}",
            outcome.recorded,
            path.display()
        )?;
        writeln!(
            out,
            "  Replay it with:  polis replay \"{}\"",
            path.display()
        )?;
    }
    if let Some(transcript) = transcript_from_this_run(repo, outcome.before) {
        writeln!(out)?;
        writeln!(out, "  Watch this session back on the map:")?;
        writeln!(out, "    polis replay \"{}\"", transcript.display())?;
    } else {
        writeln!(out)?;
        writeln!(
            out,
            "  That agent left no transcript in this repository.\n  \
             To watch one you already have:  polis watch"
        )?;
    }
    if outcome.window_open {
        writeln!(out)?;
        writeln!(
            out,
            "  The map window is still open; close it when you are done."
        )?;
    }
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

/// Which program to launch, and with what arguments.
///
/// `polis run` with nothing after it means `claude`, because that is what the
/// operator meant and asking them to type it twice is a worse product.
fn resolve_agent(args: &RunArgs) -> anyhow::Result<(PathBuf, Vec<OsString>)> {
    let mut passthrough = args.command.clone();
    let name = if passthrough.is_empty() {
        OsString::from(DEFAULT_AGENT)
    } else {
        passthrough.remove(0)
    };
    let name = name.to_string_lossy().into_owned();
    let program = setup::which(&name).ok_or_else(|| {
        if name == DEFAULT_AGENT {
            anyhow::anyhow!(
                "no `claude` on PATH.\n\nInstall Claude Code from \
                 https://claude.com/claude-code, or launch a different program with\n  \
                 polis run -- <program> [arguments]"
            )
        } else {
            anyhow::anyhow!("no `{name}` on PATH")
        }
    })?;
    Ok((program, passthrough))
}

/// Opens the map in a second process.
///
/// Silenced deliberately: the window prints its adapter line and wgpu prints its
/// own warnings, and both would land in the middle of the agent's UI.
fn spawn_window(repo: &Path) -> anyhow::Result<Child> {
    let exe = std::env::current_exe().context("finding this executable")?;
    Command::new(exe)
        .arg("--repo")
        .arg(repo)
        .arg("map")
        .env(WINDOW_CHILD_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning the map window")
}

/// Keeps the bus empty, and writes the recording when one was asked for.
///
/// Returns how many events were written. An I/O failure on the recording stops
/// the writing and not the run: the agent is the point and a full disk must not
/// take it down.
fn drain_bus(
    source: &polis_ingest::EventSource,
    record: Option<&Path>,
    clock: RecordingClock,
) -> anyhow::Result<u64> {
    let mut sink = match record {
        None => None,
        Some(path) => {
            let file = std::fs::File::create(path)
                .with_context(|| format!("creating {}", path.display()))?;
            let mut writer = io::BufWriter::new(file);
            writeln!(
                writer,
                "{}",
                serde_json::to_string(&RecordingHeader::for_clock(clock))?
            )?;
            Some(writer)
        }
    };
    let mut written = 0u64;
    while let Some(event) = source.recv() {
        let Some(writer) = sink.as_mut() else {
            continue;
        };
        let recorded = clock.record_ref(&event);
        let Ok(line) = recorded.to_json_line() else {
            continue;
        };
        if writeln!(writer, "{line}").is_err() {
            sink = None;
            continue;
        }
        written += 1;
    }
    if let Some(mut writer) = sink {
        let _ = writer.flush();
    }
    Ok(written)
}

/// Every transcript that exists right now, as a set.
///
/// Taken before the agent starts, so the one it writes can be identified by
/// being **new** rather than by being recent. Headers-only: 67 ms over 187
/// sessions on the reference machine, and it runs before the agent, so it costs
/// the operator that much and nothing else.
fn transcripts_now() -> BTreeSet<PathBuf> {
    let Some(dir) = polis_world::sessions::default_projects_dir() else {
        return BTreeSet::new();
    };
    polis_world::sessions::SessionIndex::scan_with(
        &dir,
        &polis_world::sessions::IndexOptions::quick(),
    )
    .map(|index| {
        index
            .sessions
            .iter()
            .map(|s| s.transcript.clone())
            .collect()
    })
    .unwrap_or_default()
}

/// The transcript **this run** produced, if it can be identified.
///
/// Identified by not having existed when the run started, which is the only
/// test that survives the situation Polis exists for: several agents in one
/// checkout at once. "The most recently modified session in this repository" is
/// wrong there, and it is wrong in a way that looks right — it names a real
/// session, of somebody else's work. It was wrong the first time this command
/// was run against a real machine, and it named the transcript of the agent that
/// was writing this code.
///
/// A resumed session existed before and so is not claimed. That is a false
/// negative, and a false negative here prints a generic line instead of a
/// specific one, where a false positive would send the operator to watch the
/// wrong recording.
fn transcript_from_this_run(repo: &Path, before: &BTreeSet<PathBuf>) -> Option<PathBuf> {
    let dir = polis_world::sessions::default_projects_dir()?;
    let index = polis_world::sessions::SessionIndex::scan_with(
        &dir,
        &polis_world::sessions::IndexOptions::quick(),
    )
    .ok()?;
    index
        .sessions
        .iter()
        .find(|s| {
            s.is_replayable() && s.repo.as_deref() == Some(repo) && !before.contains(&s.transcript)
        })
        .map(|s| s.transcript.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::RunArgs;

    fn args(command: &[&str]) -> RunArgs {
        RunArgs {
            no_window: true,
            record: None,
            otlp_addr: polis_ingest::default_otlp_addr(),
            command: command.iter().map(OsString::from).collect(),
        }
    }

    /// `polis run` with nothing after it means `claude`, and the error when
    /// there is no `claude` has to say what to do about it rather than repeating
    /// the operating system's message.
    #[test]
    fn the_default_agent_is_claude_and_a_missing_one_says_where_to_get_it() {
        let resolved = resolve_agent(&args(&[]));
        match resolved {
            Ok((program, passthrough)) => {
                assert!(passthrough.is_empty());
                assert!(
                    program
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .is_some_and(|s| s.eq_ignore_ascii_case(DEFAULT_AGENT)),
                    "{}",
                    program.display()
                );
            }
            Err(error) => {
                let text = error.to_string();
                assert!(text.contains("claude.com/claude-code"), "{text}");
                assert!(text.contains("polis run --"), "{text}");
            }
        }
    }

    /// Arguments after the program name are passed through untouched, including
    /// the ones that look like flags to `polis` itself.
    #[test]
    fn every_argument_after_the_program_is_passed_through() {
        let exe = std::env::current_exe().unwrap();
        let exe = exe.display().to_string();
        let (program, passthrough) =
            resolve_agent(&args(&[&exe, "--help", "-p", "do a thing"])).unwrap();
        assert_eq!(program.display().to_string(), exe);
        assert_eq!(
            passthrough,
            vec![
                OsString::from("--help"),
                OsString::from("-p"),
                OsString::from("do a thing")
            ]
        );
    }

    /// The whole point of the command: the agent process gets the block, and
    /// this process does not.
    #[test]
    fn the_telemetry_block_is_the_one_polis_env_prints() {
        let env = hookenv::agent_env("http://127.0.0.1:4317");
        assert_eq!(env.len(), 12);
        for (key, _) in &env {
            assert!(
                key.starts_with("OTEL_") || key.starts_with("CLAUDE_CODE_"),
                "{key}"
            );
        }
        assert!(
            env.iter()
                .any(|(k, v)| k == "OTEL_EXPORTER_OTLP_ENDPOINT" && v == "http://127.0.0.1:4317"),
            "the endpoint must point at the receiver this command started"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA" && v == "1"),
            "without the beta traces channel there is no subagent attribution at all"
        );
        // None of it leaks into the shell that ran `polis run`.
        assert!(
            std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_err()
                || std::env::var("POLIS_TEST_ALLOW_OTEL").is_ok(),
            "polis run must not export into its own environment"
        );
    }

    /// A run with `--record` and no events still leaves a replayable file: the
    /// header alone is a valid recording, and a zero-byte file is not.
    #[test]
    fn a_recording_always_starts_with_its_header() {
        let dir = crate::testutil::scratch("run-record");
        let path = dir.join("capture.jsonl");
        let (sink, source) = polis_ingest::bus::channel(8);
        drop(sink);
        let written = drain_bus(&source, Some(&path), RecordingClock::start_now()).unwrap();
        assert_eq!(written, 0);
        let text = std::fs::read_to_string(&path).unwrap();
        let header: RecordingHeader = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert!(header.is_supported(), "{header:?}");
    }
}
