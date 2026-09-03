//! `polis run` — an agent, launched with the map already watching (PRD §4.1, §15).
//!
//! ```text
//! polis run -- claude
//! ```
//!
//! starts Claude Code as a **child process** with the telemetry environment
//! already set on it — the exact block `polis env` prints, applied to one
//! process instead of to the operator's shell — and opens a `polis watch` window
//! beside it.
//!
//! # What changed, and why it matters
//!
//! This used to be the only way to see anything, and it did not work: it started
//! the receiver, launched the agent, and then handed the event bus to a function
//! whose own doc comment read *"keeps the bus empty"*. The events were counted
//! and discarded, and the window it opened drew a static city. An operator ran a
//! real agent for minutes and the map showed nothing.
//!
//! It is now an **enrichment of [`crate::watch`]**, which is the front door.
//! `polis watch` on its own already sees every agent in a repository, including
//! ones started in another terminal, with no configuration at all — because
//! every Claude Code session writes a transcript carrying its `cwd`. What this
//! command adds is the two channels that need setting up:
//!
//! * **Telemetry**, by setting twelve variables on the agent process — token
//!   counts, cost, and the beta traces channel that is the only thing that
//!   attributes a tool call to a subagent.
//! * **Hooks**, when they are installed, which are sub-second where a transcript
//!   append is not, and which carry `Stop` — the only signal that proves a
//!   session *ended* rather than merely going quiet.
//!
//! # Who owns the receivers
//!
//! The **window** does. That is what makes this an enrichment rather than a
//! second, parallel ingest path: there is one stack, in the process that draws,
//! and this command's job is to start an agent that talks to it. The two
//! exceptions are `--no-window` and `--record`, where there is either no window
//! to own them or a recording only this process can write; the window is then
//! spawned without the two channels that bind ports, so it still shows the
//! zero-setup live view instead of a dead city.
//!
//! # Why the agent is in the foreground and the window is the child
//!
//! It could be the other way round — `winit` owns the main thread, so the window
//! would have to be here and the agent on a thread. Three things decide it:
//!
//! 1. **The agent is a full-screen terminal application.** It needs this
//!    terminal's stdin, stdout and stderr, unredirected, with the real console
//!    handle behind them.
//! 2. **The exit code is the agent's.** `polis run -- claude -p "…"` in a script
//!    has to behave like `claude -p "…"`, including when it fails.
//! 3. **The window prints to stderr** — the adapter line, wgpu's own warnings —
//!    and that would land in the middle of the agent's redrawing UI. As a child
//!    with null stdio it cannot.
//!
//! So the window is spawned as `polis watch`, silenced, and outlives the
//! command: closing the agent does not take the map away from you mid-thought.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use polis_events::{PathMapper, RecordingClock, RecordingHeader, DEFAULT_HOOK_PORT};
use polis_ingest::env as hookenv;
use polis_ingest::{Ingest, IngestConfig, SessionScope};

use crate::cli::{Cli, RunArgs};
use crate::setup;
use crate::status::Connectivity;

/// The agent `polis run` launches when the operator names none.
pub const DEFAULT_AGENT: &str = "claude";

/// Marks the map window Polis spawned for itself.
///
/// Two uses, and both are about a process with nobody at it: it is what
/// [`setup::should_pause`] checks so that window can never sit waiting for a
/// keypress, and it is how an operator looking at two `polis` processes in a
/// task manager can tell which one they started.
pub const WINDOW_CHILD_ENV: &str = "POLIS_WINDOW_CHILD";

/// How long the agent is held back while the window's receiver comes up.
///
/// An exporter that finds nothing listening drops its batch, and the first batch
/// is the session start. The window has to open a GPU surface and read `git log`
/// first, so this is generous — and it is a *timeout*, not a requirement: on
/// expiry the agent starts anyway and the transcript channel, which needs
/// nothing, still carries the session.
pub const RECEIVER_READY_TIMEOUT: Duration = Duration::from_secs(12);

/// How often the readiness probe retries.
const READY_POLL: Duration = Duration::from_millis(100);

/// Runs an agent with Polis watching. Returns the agent's exit code.
///
/// Every step prints one line, because the operator is about to lose the screen
/// to a full-screen agent UI and this is their only chance to see what was set
/// up for them.
pub fn run(cli: &Cli, args: &RunArgs) -> anyhow::Result<i32> {
    let repo = cli.repo_root().context("resolving the repository root")?;
    let (program, passthrough) = resolve_agent(args)?;
    let env = hookenv::agent_env(&format!("http://{}", args.otlp_addr));

    // Who owns the two ports. Normally the window, which is what makes this
    // command an enrichment of `polis watch` rather than a second ingest path.
    // A recording is the exception: only this process can write it.
    let plan = Plan::decide(args);
    let mut ingest = None;
    let mut drain = None;
    if plan.parent_receives {
        let mapper = PathMapper::new(&repo).unwrap_or_default();
        let mut config = IngestConfig::new(&repo).watching(SessionScope::ThisRepo);
        config.otlp_addr = args.otlp_addr;
        let (started, source) = Ingest::start(config, mapper);
        let record = args.record.clone();
        let clock = RecordingClock::start_now();
        drain = std::thread::Builder::new()
            .name("polis-run-drain".to_owned())
            .spawn(move || drain_bus(&source, record.as_deref(), clock))
            .ok();
        ingest = Some(started);
    }

    let window = if plan.open_window {
        match already_watching() {
            Some(existing) => Window::Existing(existing),
            None => match spawn_window(&repo, plan.window_channels) {
                Ok(child) => Window::Spawned(child),
                Err(error) => Window::Failed(error.to_string()),
            },
        }
    } else {
        Window::None
    };
    banner(
        &repo,
        &program,
        args,
        plan,
        ingest.as_ref(),
        env.len(),
        &window,
    )?;

    // The agent is held back until something is listening on 4317, because an
    // exporter that finds nothing there drops its first batch and the first
    // batch is the session start. Never fatal — the zero-setup channel needs no
    // receiver at all.
    let waited = if plan.parent_receives {
        Some(Duration::ZERO)
    } else if matches!(window, Window::Spawned(_) | Window::Existing(_)) {
        wait_for_receiver(args.otlp_addr, RECEIVER_READY_TIMEOUT)
    } else {
        None
    };
    report_readiness(waited, plan.parent_receives)?;

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

    let totals = ingest.as_ref().map(Ingest::totals);
    let dropped = ingest.as_ref().map_or(0, |i| i.stats().dropped_total());
    if let Some(ingest) = ingest {
        ingest.shutdown();
    }
    let recorded = drain.map_or(0, |d| d.join().unwrap_or(Ok(0)).unwrap_or(0));

    let code = status.code().unwrap_or(i32::from(!status.success()));
    summary(
        &repo,
        args,
        &Outcome {
            code,
            elapsed,
            before: &before,
            totals: totals.as_ref(),
            dropped,
            recorded,
            window: &window,
        },
    )?;
    Ok(code)
}

/// Who receives, and what the window is started with.
///
/// One decision made once, because the alternative is three places each
/// re-deriving "does this process hold the ports" and one of them getting it
/// wrong.
#[derive(Debug, Clone, Copy)]
struct Plan {
    /// Whether this process binds the two ports rather than the window.
    parent_receives: bool,
    /// Whether to open a window at all.
    open_window: bool,
    /// What the window is told to start.
    window_channels: WindowChannels,
}

/// What the spawned window is asked to receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowChannels {
    /// Everything: the window owns both ports.
    All,
    /// Transcripts and files only, because this process holds the ports.
    ///
    /// Still a live map — the transcript channel is the one that needs no setup
    /// and carries every tool call — just without telemetry detail and hook
    /// latency in the window's own counters.
    NoPorts,
}

impl Plan {
    fn decide(args: &RunArgs) -> Self {
        // A recording can only be written by the process that holds the bus, so
        // `--record` moves the receivers here and leaves the window the two
        // channels that bind nothing.
        let parent_receives = args.no_window || args.record.is_some();
        Self {
            parent_receives,
            open_window: !args.no_window,
            window_channels: if parent_receives {
                WindowChannels::NoPorts
            } else {
                WindowChannels::All
            },
        }
    }
}

/// What happened to the map window.
#[derive(Debug)]
enum Window {
    /// Started by this command.
    Spawned(Child),
    /// Already open — another Polis holds the hook port, with this message.
    Existing(String),
    /// Asked for and could not be started.
    Failed(String),
    /// Not asked for.
    None,
}

impl Window {
    fn is_open(&self) -> bool {
        matches!(self, Self::Spawned(_) | Self::Existing(_))
    }

    /// The line the banner prints about the window, with the detail that makes
    /// it actionable: which process to look at, or why there is none.
    fn describe(&self, channels: WindowChannels) -> String {
        match self {
            Self::Spawned(child) if channels == WindowChannels::NoPorts => format!(
                "`polis watch` opening as pid {} (sessions and files only — this process \
                 holds the two ports)",
                child.id()
            ),
            Self::Spawned(child) => format!("`polis watch` opening as pid {}", child.id()),
            Self::Existing(why) => {
                format!("already open — your agent appears in the watch that is running ({why})")
            }
            Self::Failed(error) => format!("could not be opened ({error}) — the agent still runs"),
            Self::None => "not opened (--no-window)".to_owned(),
        }
    }
}

/// What the run cost and produced, for [`summary`].
struct Outcome<'a> {
    code: i32,
    elapsed: Duration,
    /// Every transcript that existed before the agent started, so the one it
    /// wrote can be told from one another agent wrote in the same checkout
    /// while it ran.
    before: &'a BTreeSet<PathBuf>,
    /// `None` when the window owns the receivers, which is the normal case.
    totals: Option<&'a polis_ingest::BusTotals>,
    dropped: u64,
    recorded: u64,
    window: &'a Window,
}

/// The lines printed before the terminal belongs to the agent.
fn banner(
    repo: &Path,
    program: &Path,
    args: &RunArgs,
    plan: Plan,
    ingest: Option<&Ingest>,
    variables: usize,
    window: &Window,
) -> anyhow::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out)?;
    writeln!(out, "  P O L I S   ·   your agent, with the map watching")?;
    writeln!(out)?;
    writeln!(out, "    repo        {}", repo.display())?;
    writeln!(out, "    agent       {}", program.display())?;
    writeln!(
        out,
        "    telemetry   {variables} variables set on that one process — not on your shell"
    )?;
    writeln!(
        out,
        "    receiver    {}",
        if plan.parent_receives {
            format!(
                "this process ({} telemetry, 127.0.0.1:{} hooks)",
                args.otlp_addr, DEFAULT_HOOK_PORT
            )
        } else {
            "the map window".to_owned()
        }
    )?;
    writeln!(
        out,
        "    window      {}",
        window.describe(plan.window_channels)
    )?;
    if let Some(path) = &args.record {
        writeln!(out, "    recording   {}", path.display())?;
    }
    for (channel, health) in ingest.map(Ingest::health).unwrap_or_default() {
        if let Some(reason) = health.reason() {
            writeln!(out, "    warning     {channel}: {reason}")?;
        }
    }
    if let Some(conflict) = ingest.and_then(Ingest::hook_conflict) {
        writeln!(out, "    warning     {conflict}")?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "  You do not need this command to see an agent. `polis watch` already shows\n  \
         every agent working here, however it was started. This adds telemetry to\n  \
         the one it launches."
    )?;
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

/// Says how long the agent was held back, when it was held back at all.
fn report_readiness(waited: Option<Duration>, parent_receives: bool) -> anyhow::Result<()> {
    let Some(waited) = waited else {
        return Ok(());
    };
    if parent_receives || waited.is_zero() {
        return Ok(());
    }
    let mut out = io::stdout().lock();
    if waited >= RECEIVER_READY_TIMEOUT {
        writeln!(
            out,
            "  note: the window's receiver did not come up within {}s. The agent starts\n  \
             anyway and still appears on the map — the transcript channel needs no\n  \
             receiver at all — but its first telemetry batch is lost.",
            RECEIVER_READY_TIMEOUT.as_secs()
        )?;
    } else {
        writeln!(
            out,
            "  waited {} for the window's receiver.",
            crate::format::duration(waited)
        )?;
    }
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
    match outcome.totals {
        Some(totals) => {
            writeln!(
                out,
                "  Polis received {} events — telemetry {}, hooks {}, files {}, sessions {}{}",
                totals.received_total(),
                totals.received(polis_events::Channel::Otel),
                totals.received(polis_events::Channel::Hook),
                totals.received(polis_events::Channel::Fs),
                totals.received(polis_events::Channel::Transcript),
                if outcome.dropped == 0 {
                    String::new()
                } else {
                    format!(" ({} dropped)", outcome.dropped)
                }
            )?;
            if totals.received_total() == 0 {
                writeln!(out)?;
                writeln!(
                    out,
                    "  Nothing arrived. `polis doctor` checks every reason that can be."
                )?;
            }
        }
        None if outcome.window.is_open() => {
            // The window has the counters, because the window has the receivers.
            // What this process can still say for itself is the roster, which is
            // read from the transcripts on disk and needs nothing running.
            let report = Connectivity::inspect(repo, SessionScope::ThisRepo);
            writeln!(out, "  {}", report.headline())?;
        }
        None => {}
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
    }
    if outcome.window.is_open() {
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

/// Whether a Polis is already receiving on this machine, and its message.
///
/// The hook port is the singleton lock (ADR-0026), so a bind failure there means
/// a watch is already up. Spawning a second window would give the operator two
/// maps of the same repository, one of which could bind neither port — so this
/// command does not, and says which one to look at instead.
fn already_watching() -> Option<String> {
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, DEFAULT_HOOK_PORT);
    match UdpSocket::bind(addr) {
        Ok(socket) => {
            drop(socket);
            None
        }
        Err(error) => Some(error.to_string()),
    }
}

/// Opens `polis watch` in a second process.
///
/// Silenced deliberately: the window prints its adapter line and wgpu prints its
/// own warnings, and both would land in the middle of the agent's UI.
fn spawn_window(repo: &Path, channels: WindowChannels) -> anyhow::Result<Child> {
    let exe = std::env::current_exe().context("finding this executable")?;
    let mut command = Command::new(exe);
    command.arg("--repo").arg(repo).arg("watch");
    if channels == WindowChannels::NoPorts {
        // This process holds both ports. The window still gets the channel that
        // makes a watch work with no setup at all.
        command.args(["--without", "otel", "--without", "hook"]);
    }
    command
        .env(WINDOW_CHILD_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning the map window")
}

/// Blocks until something accepts a connection on `addr`, or the timeout.
///
/// Returns how long it waited. A plain `TcpStream::connect` rather than a gRPC
/// handshake: the question is whether the socket is bound, and the answer is
/// whether the SYN is answered.
fn wait_for_receiver(addr: SocketAddr, timeout: Duration) -> Option<Duration> {
    let started = Instant::now();
    loop {
        if TcpStream::connect_timeout(&addr, READY_POLL).is_ok() {
            return Some(started.elapsed());
        }
        if started.elapsed() >= timeout {
            return Some(started.elapsed());
        }
        std::thread::sleep(READY_POLL);
    }
}

/// Keeps the bus empty, and writes the recording when one was asked for.
///
/// Only reached when this process holds the receivers — `--no-window` or
/// `--record`. In the normal case the window owns the bus and applies every
/// event to the world, which is the whole of PRD §15 M3.
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
/// session, of somebody else's work.
///
/// A resumed session existed before and so is not claimed. That is a false
/// negative, and a false negative here prints one line fewer, where a false
/// positive would send the operator to watch the wrong recording.
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

    /// The window owns the receivers, which is what makes this command an
    /// enrichment of `polis watch` rather than a second ingest path. The two
    /// exceptions each have a reason, and neither costs the live map.
    #[test]
    fn the_window_receives_unless_this_process_has_to() {
        let normal = Plan::decide(&RunArgs {
            no_window: false,
            ..args(&[])
        });
        assert!(!normal.parent_receives, "the window owns the ports");
        assert!(normal.open_window);
        assert_eq!(normal.window_channels, WindowChannels::All);

        // A recording can only be written by the process holding the bus — but
        // the window still opens, and still gets the channel that works with no
        // setup, so `--record` never buys a dead map.
        let recording = Plan::decide(&RunArgs {
            no_window: false,
            record: Some(PathBuf::from("capture.jsonl")),
            ..args(&[])
        });
        assert!(recording.parent_receives);
        assert!(recording.open_window);
        assert_eq!(recording.window_channels, WindowChannels::NoPorts);

        let headless = Plan::decide(&args(&[]));
        assert!(headless.parent_receives);
        assert!(!headless.open_window);
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

    /// The readiness probe answers rather than hanging when nothing will ever
    /// listen, and it bounds the wait it reports.
    #[test]
    fn waiting_for_a_receiver_that_never_arrives_gives_up_and_says_so() {
        // Port 1 on loopback: reserved, never bound, refuses immediately.
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let waited = wait_for_receiver(addr, Duration::from_millis(300)).unwrap();
        assert!(
            waited >= Duration::from_millis(250) && waited < Duration::from_secs(5),
            "{waited:?}"
        );
    }

    /// And returns at once when something is already listening.
    #[test]
    fn waiting_for_a_receiver_that_is_already_up_returns_immediately() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let waited = wait_for_receiver(addr, Duration::from_secs(5)).unwrap();
        assert!(waited < Duration::from_secs(1), "{waited:?}");
    }
}
