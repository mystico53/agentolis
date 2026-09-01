//! The subcommands that need no window (PRD §15 M0, §4.1, §4.2).
//!
//! Every command here writes to stdout and returns; none of them opens a
//! window, and none of them depends on `polis-render`. That is what makes
//! `polis tail` usable over ssh, in CI and inside a test harness — which is what
//! PRD §15 M0 asks for: *"`polis tail` prints a normalized event stream to
//! stdout. No graphics."*

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _};
use polis_events::{Channel, Event, PathMapper, RecordedEvent, RecordingClock, RecordingHeader};
use polis_ingest::env::{self as hookenv, HookInstall};
use polis_ingest::transcript::{ParseStats, Replay};
use polis_ingest::{BusStats, BusTotals, EventSource, Ingest, IngestConfig, SourceHealth};

use crate::cli::{Cli, EnvArgs, InstallHooksArgs, ReplayArgs, ShellArg, TailArgs};
use crate::format;

/// How long the consumer loop sleeps when the bus is empty.
///
/// `EventSource::recv_batch` blocks until an event arrives, which is right for
/// the world thread and wrong here: `polis tail` has a `--duration`, a `--limit`
/// and a counters line, all of which need a wakeup on an idle repository. So the
/// loop uses the non-blocking `drain` and sleeps this long when it returns
/// nothing — five wakeups a second, far inside PRD §13.1's under-2%-of-a-core
/// idle budget, and *not* the spin that budget rules out. 200 ms is well under
/// the 1 s log-export interval, so it never delays a batch that has arrived.
const TICK: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// polis tail — PRD §15 M0
// ---------------------------------------------------------------------------

/// Prints the normalized event stream to stdout.
///
/// Starts all four channels, each independently fallible (ADR-0011), and drains
/// the bus on this thread. The `--source` filter applies to the *output* only:
/// every channel keeps running and keeps counting, because a filtered-out
/// channel and a dead one must not look the same.
pub fn tail(cli: &Cli, args: &TailArgs) -> anyhow::Result<()> {
    let repo_root = cli.repo_root().context("resolving the repository root")?;
    let mapper = PathMapper::new(&repo_root).map_err(|e| {
        anyhow!(
            "{} is not a usable repository root: {e}",
            repo_root.display()
        )
    })?;

    let mut config = IngestConfig::new(&repo_root);
    config.otlp_addr = args.otlp_addr;
    config.hook_addr = args.hook_addr;
    config.channels = args.started();
    if let Some(projects) = &args.projects {
        config.claude_projects_dir.clone_from(projects);
    }

    let mut out = BufWriter::new(std::io::stdout().lock());
    writeln!(out, "# polis tail — repo {}", repo_root.display())?;
    writeln!(
        out,
        "# otlp {} | hooks {} | transcripts {}",
        config.otlp_addr,
        config.hook_addr,
        config.claude_projects_dir.display()
    )?;

    let (ingest, source) = Ingest::start(config, mapper);

    // ADR-0026: two daemons would each see roughly half the hook traffic and
    // neither would say so, so this is a clear exit rather than a warning —
    // unless the operator has explicitly said they want a second one.
    // Owned before `shutdown` moves the stack: the message has to outlive the
    // handle it came from.
    if let Some(conflict) = ingest.hook_conflict().map(ToOwned::to_owned) {
        if !args.allow_second {
            out.flush()?;
            ingest.shutdown();
            return Err(anyhow!(
                "{conflict}\n\nRun with --allow-second to tail anyway without Channel B, or stop \
                 the other Polis first."
            ));
        }
        writeln!(out, "# WARNING: {conflict}")?;
    }
    for (channel, health) in ingest.health() {
        writeln!(out, "# channel {channel:<11} {health}")?;
    }
    if let Some(path) = ingest.endpoint_file() {
        writeln!(out, "# hook endpoint published at {}", path.display())?;
    }
    // One clock for the whole run, created here rather than inside `drain` so
    // the recording's header and its events share an origin. `--json` writes
    // that header as its last preamble line: without it the capture is a bare
    // stream of `RecordedEvent`s with no `wall_origin`, which `polis replay`
    // cannot tell from a Claude Code transcript — and used to misread as one.
    let clock = RecordingClock::start_now();
    if args.json {
        writeln!(
            out,
            "{}",
            serde_json::to_string(&RecordingHeader::for_clock(clock))?
        )?;
    } else {
        writeln!(out, "{}", format::header())?;
    }
    out.flush()?;

    let shown = args.shown();
    let report = drain(&source, args, &shown, clock, &mut out)?;
    let health = ingest.health();
    ingest.shutdown();

    report.print(&mut out, &health)?;
    out.flush()?;
    Ok(())
}

/// What a `tail` run measured.
#[derive(Debug, Clone, Copy)]
struct Report {
    elapsed: Duration,
    received: u64,
    printed: u64,
    stats: BusStats,
    totals: BusTotals,
}

impl Report {
    fn rate(&self) -> f64 {
        let seconds = self.elapsed.as_secs_f64();
        if seconds <= 0.0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            let received = self.received as f64;
            received / seconds
        }
    }

    fn print(
        &self,
        out: &mut impl Write,
        health: &[(Channel, SourceHealth)],
    ) -> std::io::Result<()> {
        writeln!(out, "#")?;
        writeln!(
            out,
            "# {:.3}s | {} events received, {} printed | {:.1} events/sec",
            self.elapsed.as_secs_f64(),
            self.received,
            self.printed,
            self.rate()
        )?;
        writeln!(out, "# {}", counters(&self.stats, &self.totals))?;
        // PRD §4.5: dropping is correct — backpressure must never reach an
        // agent — but it must be visible, so it is stated even when it is zero.
        if self.stats.dropped_total() == 0 {
            writeln!(out, "# dropped: 0 (PRD §13.1's target)")?;
        } else {
            writeln!(
                out,
                "# DROPPED {} EVENTS — the bus is full and the world thread is behind",
                self.stats.dropped_total()
            )?;
        }
        for (channel, state) in health {
            if !state.is_healthy() {
                writeln!(out, "# channel {channel} ended {state}")?;
            }
        }
        Ok(())
    }
}

/// The consumer loop. See [`TICK`] for why it polls rather than blocks.
fn drain(
    source: &EventSource,
    args: &TailArgs,
    shown: &[Channel],
    clock: RecordingClock,
    out: &mut impl Write,
) -> anyhow::Result<Report> {
    let started = Instant::now();
    let deadline = args.duration.map(|s| started + Duration::from_secs(s));
    let counter_period =
        (args.counters_every > 0).then(|| Duration::from_secs(args.counters_every));
    let mut next_counters = counter_period.map(|p| started + p);

    let mut batch: Vec<Event> = Vec::new();
    let mut received = 0u64;
    let mut printed = 0u64;

    loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        if args.limit.is_some_and(|n| received >= n) {
            break;
        }
        batch.clear();
        // `drain` is bounded by the bus capacity, so a fast producer cannot hold
        // this loop inside one call; see `TICK` for why this is not `recv_batch`.
        if source.drain(&mut batch) == 0 {
            tick_counters(out, source, &mut next_counters, counter_period)?;
            std::thread::sleep(TICK);
            continue;
        }
        for event in &batch {
            received += 1;
            if !shown.contains(&event.meta.channel) {
                continue;
            }
            printed += 1;
            if args.json {
                // Never a bare `Event`: `EventMeta.observed` is `#[serde(skip)]`,
                // so serializing one loses its timing entirely. `RecordingClock`
                // is the supported path and its output is what `polis replay`
                // reads back (ADR-0049).
                writeln!(out, "{}", clock.record_ref(event).to_json_line()?)?;
            } else {
                // The event's OWN observed time, not `started.elapsed()`. A
                // batch is drained and printed in one go, so the printer's
                // clock stamps every event in it with the same value — six
                // hook spawns 30 ms apart all rendered as `3.205`. Worse, it
                // disagreed with `--json` and with `polis replay`, which both
                // key off `meta.observed` through this same `RecordingClock`;
                // a `tail --json` capture replayed did not line up with the
                // `tail` that produced it. One clock, one number, three
                // renderers.
                #[allow(clippy::cast_precision_loss)]
                let seconds = clock.offset_ms(event.meta.observed) as f64 / 1000.0;
                writeln!(out, "{}", format::line(seconds, &format::describe(event)))?;
            }
        }
        out.flush()?;
        tick_counters(out, source, &mut next_counters, counter_period)?;
    }

    Ok(Report {
        elapsed: started.elapsed(),
        received,
        printed,
        stats: source.stats(),
        totals: source.totals(),
    })
}

fn tick_counters(
    out: &mut impl Write,
    source: &EventSource,
    next: &mut Option<Instant>,
    period: Option<Duration>,
) -> std::io::Result<()> {
    let (Some(due), Some(period)) = (*next, period) else {
        return Ok(());
    };
    if Instant::now() < due {
        return Ok(());
    }
    writeln!(out, "# {}", counters(&source.stats(), &source.totals()))?;
    out.flush()?;
    *next = Some(Instant::now() + period);
    Ok(())
}

/// The per-channel received/dropped line (PRD §4.5).
fn counters(stats: &BusStats, totals: &BusTotals) -> String {
    let mut received = String::new();
    let mut dropped = String::new();
    for channel in [
        Channel::Otel,
        Channel::Hook,
        Channel::Fs,
        Channel::Transcript,
        Channel::Control,
    ] {
        use std::fmt::Write as _;
        let _ = write!(received, " {channel}={}", totals.received(channel));
        let _ = write!(dropped, " {channel}={}", stats.dropped(channel));
    }
    format!(
        "received{received} (total {}) | dropped{dropped} (total {}) | queued {}/{}",
        totals.received_total(),
        stats.dropped_total(),
        stats.queued,
        stats.capacity
    )
}

// ---------------------------------------------------------------------------
// polis install-hooks — PRD §4.2
// ---------------------------------------------------------------------------

/// Writes (or shows) the `.claude/settings.json` hooks block.
pub fn install_hooks(cli: &Cli, args: &InstallHooksArgs) -> anyhow::Result<()> {
    let repo_root = cli.repo_root().context("resolving the repository root")?;
    let settings = match (&args.settings, args.user) {
        (Some(path), _) => path.clone(),
        (None, true) => user_settings_path()
            .ok_or_else(|| anyhow!("no home directory to place ~/.claude/settings.json in"))?,
        (None, false) => repo_root.join(".claude").join("settings.json"),
    };
    let hook_binary = match &args.hook_binary {
        Some(path) => path.clone(),
        None => find_hook_binary(&repo_root),
    };

    let mut out = BufWriter::new(std::io::stdout().lock());
    writeln!(out, "settings:    {}", settings.display())?;
    writeln!(out, "polis-hook:  {}", hook_binary.display())?;
    if !hook_binary.exists() {
        writeln!(
            out,
            "WARNING:     that file does not exist yet — build it with \
             `cargo build --profile hook -p polis-hook`"
        )?;
    }
    writeln!(out)?;

    let plan = hookenv::plan_hooks(&settings, &hook_binary);
    report_plan(&mut out, &plan)?;

    if plan.blocked {
        out.flush()?;
        return Err(anyhow!(
            "refusing to touch {}; fix or move it and re-run",
            settings.display()
        ));
    }
    if args.dry_run {
        writeln!(out, "\n--- {} (would write) ---", settings.display())?;
        writeln!(out, "{}", plan.settings_json)?;
        out.flush()?;
        return Ok(());
    }
    if plan.has_conflicts() && !args.force {
        out.flush()?;
        return Err(anyhow!(
            "{} already registers {} that Polis did not write; re-run with --force to replace \
             them, or with --dry-run to see the merge",
            settings.display(),
            plan.conflicts.join(", ")
        ));
    }

    let done = hookenv::install_hooks(&settings, &hook_binary);
    if !done.written {
        out.flush()?;
        return Err(anyhow!("nothing was written: {}", done.notes.join("; ")));
    }
    if let Some(backup) = &done.backup {
        writeln!(out, "\nbacked up the previous file to {}", backup.display())?;
    }
    writeln!(out, "wrote {}", settings.display())?;
    out.flush()?;
    Ok(())
}

fn report_plan(out: &mut impl Write, plan: &HookInstall) -> std::io::Result<()> {
    writeln!(
        out,
        "{} registrations: {} new, {} already correct, {} conflicting",
        plan.added.len() + plan.unchanged.len() + plan.conflicts.len(),
        plan.added.len(),
        plan.unchanged.len(),
        plan.conflicts.len()
    )?;
    if !plan.conflicts.is_empty() {
        writeln!(
            out,
            "CONFLICT:    {} already has handlers Polis did not write",
            plan.conflicts.join(", ")
        )?;
    }
    for note in &plan.notes {
        writeln!(out, "note:        {note}")?;
    }
    Ok(())
}

/// `~/.claude/settings.json`.
fn user_settings_path() -> Option<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .or_else(|| std::env::var_os(if cfg!(windows) { "HOME" } else { "USERPROFILE" }))?;
    Some(PathBuf::from(home).join(".claude").join("settings.json"))
}

/// Finds `polis-hook`, preferring the copy that shipped with this `polis`.
///
/// On Windows exec form requires a real executable — a `.cmd` shim cannot be
/// spawned without a shell — so this only ever returns a `.exe`
/// (`docs/verified/hooks-schema.md` §5.3).
fn find_hook_binary(repo_root: &Path) -> PathBuf {
    let exe_name = if cfg!(windows) {
        "polis-hook.exe"
    } else {
        "polis-hook"
    };
    let mut candidates = Vec::new();
    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            candidates.push(dir.join(exe_name));
        }
    }
    // `--profile hook` is the profile PRD §4.2's budget was measured against.
    for profile in ["hook", "release", "debug"] {
        candidates.push(repo_root.join("target").join(profile).join(exe_name));
    }
    for candidate in &candidates {
        if candidate.is_file() {
            return crate::cli::strip_verbatim(
                candidate
                    .canonicalize()
                    .unwrap_or_else(|_| candidate.clone()),
            );
        }
    }
    // Not an error: an operator may be generating a settings file for a machine
    // where the binary lives somewhere else. The bare name resolves on `PATH`,
    // which is exactly what exec form does with it.
    PathBuf::from(exe_name)
}

// ---------------------------------------------------------------------------
// polis env — PRD §4.1
// ---------------------------------------------------------------------------

/// Prints the telemetry environment block for an agent Polis did not launch.
pub fn env(args: &EnvArgs) -> anyhow::Result<()> {
    let mut out = BufWriter::new(std::io::stdout().lock());
    if !args.bare {
        writeln!(
            out,
            "# The block Claude Code needs for Polis to see anything at all."
        )?;
        writeln!(
            out,
            "# Verified end to end in docs/verified/otel-schema.md §1.6."
        )?;
        writeln!(out, "#")?;
        writeln!(
            out,
            "# CLAUDE_CODE_ENHANCED_TELEMETRY_BETA and OTEL_TRACES_EXPORTER are REQUIRED, not"
        )?;
        writeln!(
            out,
            "# optional: the logs channel carries no agent discriminator at all, so without a"
        )?;
        writeln!(
            out,
            "# claude_code.tool span there is no way to tell a subagent's edit from the main"
        )?;
        writeln!(out, "# agent's, and Polis says so rather than guessing.")?;
        writeln!(out, "#")?;
    }
    match args.shell {
        ShellArg::Sh => write!(out, "{}", hookenv::export_block_sh(&args.endpoint))?,
        ShellArg::Powershell => {
            write!(out, "{}", hookenv::export_block_powershell(&args.endpoint))?;
        }
        ShellArg::Env => {
            for (key, value) in hookenv::agent_env(&args.endpoint) {
                writeln!(out, "{key}={value}")?;
            }
        }
    }
    if !args.bare {
        writeln!(out, "#")?;
        writeln!(out, "# Deliberately NOT set (ADR-0005):")?;
        for (name, why) in hookenv::SUPPRESSED_ENV {
            writeln!(out, "#   {name} — {why}")?;
        }
    }
    out.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// polis replay — PRD §15 M2's foundation
// ---------------------------------------------------------------------------

/// Reads a Polis recording — the format `polis tail --json` and
/// `polis replay --json` write (ADR-0049) — or `None` when this is not one.
///
/// Sniffed rather than switched on the extension, because a capture is
/// conventionally `.jsonl` too and the operator should not have to say which
/// kind of JSON Lines file they have. The sniff is safe in both directions: a
/// [`RecordingHeader`] requires `format`, `wall_origin` and `producer`, none of
/// which appears on any of the 19 Claude Code transcript record types, and a
/// transcript record's `type` is not a field the header has.
///
/// `#` comment lines are skipped: `polis tail --json` writes its preamble as
/// comments so that one capture is both readable and replayable.
fn read_recording(path: &Path) -> anyhow::Result<Option<Replay>> {
    // Bytes then lossy, never `read_to_string`: a transcript is free to contain
    // invalid UTF-8 (PRD §16 requires the parser to degrade, not fail), and a
    // hard error here would turn a file the transcript reader handles perfectly
    // well into a dead command before it ever reached it.
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));

    let Some(first) = lines.next() else {
        return Ok(None);
    };
    let Ok(header) = serde_json::from_str::<RecordingHeader>(first) else {
        return Ok(None);
    };
    // Recognised as a recording, so from here a problem is an error rather than
    // a silent fallback to the transcript parser: guessing at a format we have
    // already identified is exactly what this function exists to stop.
    anyhow::ensure!(
        header.is_supported(),
        "{} is a Polis recording in format {}, which this build cannot read",
        path.display(),
        header.format
    );

    let mut events = Vec::new();
    let mut stats = ParseStats::default();
    for line in lines {
        match RecordedEvent::from_json_line(line) {
            Ok(event) => {
                stats.ok += 1;
                events.push(event);
            }
            // Degrade, never abort (PRD §16): a capture truncated by Ctrl-C
            // mid-line is the common case, and the events before it are good.
            Err(_) => stats.bad_json += 1,
        }
    }
    Ok(Some(Replay {
        header,
        events,
        stats,
        files: Vec::new(),
    }))
}

/// Reads one session offline and prints the reconstructed event stream.
///
/// Ordered by `(file, byte_offset)` and never by the records' own timestamps:
/// 20% of transcript files contain a backwards step and one observed jump was
/// 60 seconds (ADR-0014).
pub fn replay(cli: &Cli, args: &ReplayArgs) -> anyhow::Result<()> {
    let repo_root = cli.repo_root().context("resolving the repository root")?;
    let mapper = PathMapper::new(&repo_root).unwrap_or_default();
    let path = &args.transcript;

    // A path that is not there is a typo, not an empty session. The tailer
    // below is deliberately forgiving — a transcript can vanish under a live
    // tail and that must not be an error — so it returns Ok with zero events,
    // which on the command line renders exactly like a real but empty
    // recording and exits 0. Catch it here, where the argument came from a
    // human, rather than loosening the tailer.
    if !path.exists() {
        anyhow::bail!(
            "no such transcript: {} (expected a .jsonl file or a <session-id> directory)",
            path.display()
        );
    }

    let replay = if path.is_dir() {
        polis_ingest::transcript::read_session(path, &mapper)
            .with_context(|| format!("reading {}", path.display()))?
    } else if let Some(recording) = read_recording(path)? {
        // A `polis tail --json` / `polis replay --json` capture, not a Claude
        // Code transcript. Dispatching on `is_dir` alone sent these down the
        // transcript parser, which found no `type` field on any line and
        // rendered the whole capture as `transcript.unknown` at t+0.000 —
        // silently, since an unmodelled type is legitimately not an error.
        recording
    } else {
        polis_ingest::transcript::read_transcript(path, &mapper)
            .with_context(|| format!("reading {}", path.display()))?
    };

    let sink: Box<dyn Write> = match &args.out {
        Some(out) => Box::new(BufWriter::new(
            std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?,
        )),
        None => Box::new(BufWriter::new(std::io::stdout().lock())),
    };
    let mut out = sink;

    if args.json {
        // Exactly the on-disk recording format: one header line, then one
        // `RecordedEvent` per line (ADR-0049). `--limit` is honoured by writing
        // the prefix by hand rather than through `Replay::write_jsonl`.
        writeln!(out, "{}", serde_json::to_string(&replay.header)?)?;
        for event in take(&replay.events, args.limit) {
            writeln!(out, "{}", event.to_json_line()?)?;
        }
        out.flush()?;
        return Ok(());
    }

    writeln!(out, "# polis replay — {}", path.display())?;
    for file in &replay.files {
        writeln!(out, "# file {}", file.path.display())?;
    }
    writeln!(
        out,
        "# {} events | {} parsed, {} blank, {} bad json, {} bad shape, {} unknown type",
        replay.len(),
        replay.stats.ok,
        replay.stats.blank,
        replay.stats.bad_json,
        replay.stats.bad_shape,
        replay.stats.unknown_type
    )?;
    // A bit-exact comparison is right here: this reports whether the operator
    // typed the flag, not whether a computed rate is near one.
    if (args.speed - 1.0).abs() > f32::EPSILON {
        writeln!(
            out,
            "# --speed {} is recorded and ignored: there is no renderer yet, so the offline read \
             runs as fast as it parses (PRD §15 M2)",
            args.speed
        )?;
    }
    writeln!(out, "{}", format::header())?;
    for recorded in take(&replay.events, args.limit) {
        #[allow(clippy::cast_precision_loss)]
        let seconds = recorded.monotonic_offset_ms as f64 / 1000.0;
        writeln!(
            out,
            "{}",
            format::line(seconds, &format::describe(&recorded.event))
        )?;
    }
    if !replay.stats.unknown_types_seen.is_empty() {
        writeln!(out, "#")?;
        for (name, count) in &replay.stats.unknown_types_seen {
            writeln!(
                out,
                "# drift: {count} record(s) of unmodelled type {name:?}"
            )?;
        }
    }
    out.flush()?;
    Ok(())
}

fn take<T>(items: &[T], limit: Option<u64>) -> &[T] {
    match limit {
        None => items,
        Some(n) => {
            let n = usize::try_from(n).unwrap_or(usize::MAX);
            &items[..n.min(items.len())]
        }
    }
}

// ---------------------------------------------------------------------------
// polis doctor — PRD §4
// ---------------------------------------------------------------------------

/// Reports the environment, the hook wiring and the agent env block.
pub fn doctor(cli: &Cli) -> anyhow::Result<()> {
    let repo_root = cli.repo_root().context("resolving the repository root")?;
    let mut out = BufWriter::new(std::io::stdout().lock());

    writeln!(out, "repo root:        {}", repo_root.display())?;
    match polis_ingest::default_claude_projects_dir() {
        Some(dir) => writeln!(
            out,
            "transcripts:      {} ({})",
            dir.display(),
            if dir.is_dir() { "present" } else { "MISSING" }
        )?,
        None => writeln!(
            out,
            "transcripts:      no home directory to derive one from"
        )?,
    }
    match polis_ingest::hook_listener::endpoint_dir() {
        Some(dir) => {
            let file = dir.join("endpoint");
            let state = std::fs::read_to_string(&file).map_or_else(
                |_| "not published".to_owned(),
                |text| text.lines().next().unwrap_or_default().to_owned(),
            );
            writeln!(out, "hook endpoint:    {} ({state})", file.display())?;
        }
        None => writeln!(out, "hook endpoint:    no per-user runtime directory")?,
    }
    writeln!(
        out,
        "polis-hook:       {}",
        find_hook_binary(&repo_root).display()
    )?;
    let project_settings = repo_root.join(".claude").join("settings.json");
    writeln!(
        out,
        "project settings: {} ({})",
        project_settings.display(),
        hook_state(&project_settings)
    )?;
    if let Some(user) = user_settings_path() {
        writeln!(
            out,
            "user settings:    {} ({})",
            user.display(),
            hook_state(&user)
        )?;
    }

    writeln!(out, "\nagent environment (polis env):")?;
    for (key, value) in hookenv::agent_env("http://127.0.0.1:4317") {
        let live = std::env::var(&key).ok();
        let mark = match live.as_deref() {
            Some(current) if current == value => "ok  ",
            Some(_) => "DIFF",
            None => "unset",
        };
        writeln!(out, "  {mark:<5} {key}={value}")?;
    }
    writeln!(
        out,
        "\n`unset` is the normal state: Claude Code strips every OTEL_* variable from the\n\
         processes it spawns, so these matter in the shell that launches `claude`, not here."
    )?;
    out.flush()?;
    Ok(())
}

/// Whether a settings file registers Polis's hooks.
fn hook_state(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return "absent".to_owned();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return "PRESENT BUT NOT VALID JSON".to_owned();
    };
    let Some(hooks) = value.get("hooks").and_then(serde_json::Value::as_object) else {
        return "no hooks block".to_owned();
    };
    let ours = hooks
        .values()
        .filter(|groups| {
            groups
                .as_array()
                .is_some_and(|g| g.iter().any(handler_is_polis))
        })
        .count();
    if hooks.contains_key("WorktreeCreate") {
        return format!(
            "{ours} polis registrations — and a WorktreeCreate hook, which replaces git's \
             worktree creation (ADR-0002)"
        );
    }
    format!("{ours} of {} registrations are polis's", hooks.len())
}

fn handler_is_polis(group: &serde_json::Value) -> bool {
    group
        .get("hooks")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|handlers| {
            handlers.iter().any(|h| {
                h.get("command")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|c| c.contains("polis-hook"))
            })
        })
}

#[cfg(test)]
mod tests {
    use polis_ingest::bus;

    use super::*;

    #[test]
    fn the_counters_line_names_every_channel_even_at_zero() {
        let (sink, source) = bus::channel(8);
        sink.push_control(polis_events::ControlEvent::Shutdown);
        let line = counters(&source.stats(), &source.totals());
        for channel in ["otel", "hook", "fs", "transcript", "control"] {
            assert!(
                line.matches(channel).count() >= 2,
                "{channel} must appear in both halves: {line}"
            );
        }
        // A silent zero is the whole point: a channel that produced nothing and
        // a channel that is off must both be visible.
        assert!(line.contains("otel=0"), "{line}");
        assert!(line.contains("control=1"), "{line}");
        assert!(line.contains("dropped"), "{line}");
        assert!(line.contains("queued 1/8"), "{line}");
    }

    #[test]
    fn a_limit_never_indexes_past_the_end() {
        let items = [1, 2, 3];
        assert_eq!(take(&items, None).len(), 3);
        assert_eq!(take(&items, Some(0)).len(), 0);
        assert_eq!(take(&items, Some(2)).len(), 2);
        assert_eq!(take(&items, Some(9_999)).len(), 3);
        assert_eq!(take(&items, Some(u64::MAX)).len(), 3);
    }

    #[test]
    fn the_hook_binary_falls_back_to_a_bare_name_that_resolves_on_path() {
        let found = find_hook_binary(Path::new("/definitely/not/a/repo"));
        assert!(
            found
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("polis-hook"),
            "{}",
            found.display()
        );
    }

    #[test]
    fn hook_state_reports_a_worktree_create_registration_as_the_hazard_it_is() {
        let dir = crate::testutil::scratch("hook-state");
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"hooks":{"WorktreeCreate":[{"hooks":[{"type":"command","command":"x"}]}]}}"#,
        )
        .unwrap();
        assert!(hook_state(&path).contains("WorktreeCreate"));

        std::fs::write(&path, "{ not json").unwrap();
        assert!(hook_state(&path).contains("NOT VALID JSON"));
        assert_eq!(hook_state(&dir.join("nope.json")), "absent");
    }

    /// The defect this guards: `tail`'s text column used the *printer's*
    /// elapsed time, so every event drained in one batch rendered with the
    /// same `t+s` — six hook spawns 30 ms apart all showed `3.205` — while
    /// `--json` and `polis replay` used `meta.observed`. One clock, three
    /// renderers, or the three disagree about the same run.
    /// Builds a `tail` argument set with everything at its default.
    fn tail_args(json: bool, limit: u64) -> TailArgs {
        TailArgs {
            source: Vec::new(),
            without: Vec::new(),
            json,
            counters_every: 0,
            duration: Some(5),
            limit: Some(limit),
            otlp_addr: polis_ingest::default_otlp_addr(),
            hook_addr: std::net::SocketAddrV4::new(
                std::net::Ipv4Addr::LOCALHOST,
                polis_events::DEFAULT_HOOK_PORT,
            ),
            projects: None,
            allow_second: false,
        }
    }

    /// Two events observed 40 ms apart, delivered in ONE drained batch. This
    /// drives the real `drain`, so it fails against the printer's-clock
    /// version: that rendered both with whatever `started.elapsed()` happened
    /// to be at flush time, collapsing them to the same `t+s`.
    #[test]
    fn the_text_column_is_the_events_own_time_not_the_printers() {
        let clock = RecordingClock::start_now();
        let origin = clock.mono_origin();
        let (sink, source) = bus::channel(16);

        for ms in [10u64, 50] {
            let mut meta = polis_events::EventMeta::now(Channel::Hook);
            meta.observed = origin + Duration::from_millis(ms);
            sink.push(Event::new(
                meta,
                polis_events::Payload::Fs(polis_events::FsEvent::RescanRequired),
            ));
        }
        drop(sink);

        let mut out: Vec<u8> = Vec::new();
        let args = tail_args(false, 2);
        drain(&source, &args, &[Channel::Hook], clock, &mut out).expect("drain");
        let text = String::from_utf8(out).expect("utf-8");
        let stamps: Vec<&str> = text
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.split_whitespace().next())
            .collect();

        assert_eq!(stamps, ["0.010", "0.050"], "rendered:\n{text}");
        assert_ne!(
            stamps[0], stamps[1],
            "two events 40 ms apart in one batch must not share a timestamp"
        );
    }

    /// The other half of the same property: `--json` and the text column are
    /// the same number, because they now share one clock.
    #[test]
    fn the_json_and_text_renderings_of_one_run_agree() {
        let clock = RecordingClock::start_now();
        let (sink, source) = bus::channel(16);
        let mut meta = polis_events::EventMeta::now(Channel::Hook);
        meta.observed = clock.mono_origin() + Duration::from_millis(1_250);
        sink.push(Event::new(
            meta,
            polis_events::Payload::Fs(polis_events::FsEvent::RescanRequired),
        ));
        drop(sink);

        let mut out: Vec<u8> = Vec::new();
        drain(
            &source,
            &tail_args(true, 1),
            &[Channel::Hook],
            clock,
            &mut out,
        )
        .expect("drain");
        let line = String::from_utf8(out)
            .expect("utf-8")
            .lines()
            .find(|l| l.starts_with('{'))
            .expect("a RecordedEvent line")
            .to_owned();
        let recorded = RecordedEvent::from_json_line(&line).expect("parses");
        assert_eq!(recorded.monotonic_offset_ms, 1_250);
    }

    /// ADR-0049 claims a `tail --json` capture is replayable. It was not:
    /// `replay` dispatched on `is_dir()` alone, so a recording went to the
    /// Claude Code transcript parser and came out as `transcript.unknown` at
    /// t+0.000 for every line — silently, because an unmodelled record type is
    /// legitimately not an error.
    #[test]
    fn a_tail_json_capture_is_recognised_as_a_recording_and_replayed() {
        let dir = crate::testutil::scratch("recording");
        let path = dir.join("capture.jsonl");
        let clock = RecordingClock::start_now();

        let mut meta = polis_events::EventMeta::now(Channel::Hook);
        meta.observed = clock.mono_origin() + Duration::from_millis(1_250);
        let event = Event::new(
            meta,
            polis_events::Payload::Fs(polis_events::FsEvent::RescanRequired),
        );

        let mut text = String::new();
        // Exactly what `tail --json` writes: comment preamble, header, events.
        text.push_str("# polis tail — repo /somewhere\n");
        text.push_str("# channel hook running\n");
        text.push_str(&serde_json::to_string(&RecordingHeader::for_clock(clock)).unwrap());
        text.push('\n');
        text.push_str(&clock.record_ref(&event).to_json_line().unwrap());
        text.push('\n');
        std::fs::write(&path, &text).unwrap();

        let replay = read_recording(&path)
            .expect("readable")
            .expect("is a recording");
        assert_eq!(
            replay.events.len(),
            1,
            "the event must survive the round trip"
        );
        assert_eq!(
            replay.events[0].monotonic_offset_ms, 1_250,
            "and keep its time"
        );
        assert_eq!(replay.stats.bad_json, 0, "the # preamble is not corruption");
        assert!(replay.header.is_supported());
    }

    /// The sniff must not claim files that are not recordings — a Claude Code
    /// transcript has to keep going to the transcript parser.
    #[test]
    fn a_transcript_and_a_garbage_file_are_not_mistaken_for_recordings() {
        let dir = crate::testutil::scratch("not-recording");

        let transcript = dir.join("session.jsonl");
        std::fs::write(
            &transcript,
            "{\"type\":\"user\",\"uuid\":\"u1\",\"sessionId\":\"s\",\"message\":{}}\n",
        )
        .unwrap();
        assert!(read_recording(&transcript).unwrap().is_none());

        // Invalid UTF-8 must degrade to "not a recording", never to an error:
        // PRD §16 requires the parser to degrade, and an `Err` here would kill
        // a file the transcript reader handles fine.
        let garbage = dir.join("garbage.jsonl");
        std::fs::write(&garbage, [0xFFu8, 0xFE, 0x80, b'\n', 0xC0]).unwrap();
        assert!(read_recording(&garbage).unwrap().is_none());

        let empty = dir.join("empty.jsonl");
        std::fs::write(&empty, "").unwrap();
        assert!(read_recording(&empty).unwrap().is_none());

        // A recognised recording in an unreadable format is refused outright
        // rather than guessed at.
        let future = dir.join("future.jsonl");
        std::fs::write(
            &future,
            "{\"format\":99,\"wall_origin\":0,\"producer\":\"x\"}\n",
        )
        .unwrap();
        assert!(read_recording(&future).is_err());
    }
}
