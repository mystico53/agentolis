//! The command line (PRD §4.2, §15).

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use polis_events::{Channel, DEFAULT_HOOK_PORT};
use polis_ingest::{default_otlp_addr, ChannelSet};

/// `polis` — a live, glanceable city map of what your coding agents are doing.
#[derive(Debug, Parser)]
#[command(name = "polis", version, about)]
pub struct Cli {
    /// Repository to map. Defaults to the current directory.
    #[arg(long, short = 'C', global = true)]
    pub repo: Option<PathBuf>,

    /// What to do. Defaults to opening the window.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the normalized event stream to stdout. No graphics.
    ///
    /// PRD §15 M0's deliverable, and the way to prove the hook meets its budget
    /// under synthetic load.
    Tail(TailArgs),

    /// Write the `.claude/settings.json` hooks block (PRD §4.2).
    ///
    /// Merges key-by-key and warns before replacing any array Polis did not
    /// write. Registers 19 events in **exec form** — never a shell command,
    /// which costs 6× on Git Bash and 25× on PowerShell per event — and
    /// deliberately does **not** register `WorktreeCreate`, which would break
    /// every worktree on the machine.
    InstallHooks(InstallHooksArgs),

    /// Print the telemetry environment block an agent needs (PRD §4.1).
    ///
    /// The variables only reach agents Polis launched itself, and operators
    /// start `claude` from their own terminal — so this exists to make the
    /// block reproducible rather than folklore.
    Env(EnvArgs),

    /// Animate a recorded session over the city (PRD §15 M2).
    ///
    /// The fastest iteration loop the project has, and it ships independently as
    /// a PR-summary or standup artifact.
    Replay(ReplayArgs),

    /// Generate the city and write it to a PNG without opening a window.
    ///
    /// PRD §15 M1's gate: byte-identical layout across two runs and two machines.
    Snapshot(SnapshotArgs),

    /// Report environment, channel health and the injected agent env block.
    ///
    /// Prints the environment variables an operator must set for an agent Polis
    /// did not launch — the common case, since operators start `claude`
    /// themselves and the endpoint env var only reaches Polis's own children.
    Doctor,
}

/// `polis tail` — PRD §15 M0.
#[derive(Debug, clap::Args)]
pub struct TailArgs {
    /// Only show these channels. Repeatable; every channel by default.
    ///
    /// Filters the *output*, not the ingest: a channel left out is still
    /// started, still counted, and still visible in the counters line, because
    /// "I filtered it out" and "it produced nothing" must not look the same.
    #[arg(long = "source", value_enum, alias = "channel")]
    pub source: Vec<ChannelArg>,

    /// Do not start these channels at all. Repeatable.
    #[arg(long = "without", value_enum)]
    pub without: Vec<ChannelArg>,

    /// One JSON object per line instead of the human-readable stream.
    ///
    /// The line format is `polis_events::RecordedEvent` — a wall clock, a
    /// monotonic offset and the event — which is the same format `polis replay`
    /// reads, so a `tail --json` capture can be replayed (ADR-0049).
    #[arg(long)]
    pub json: bool,

    /// Seconds between counter lines. `0` prints none.
    #[arg(long = "counters", default_value_t = 5, value_name = "SECS")]
    pub counters_every: u64,

    /// Stop after this many seconds. Runs until interrupted by default.
    #[arg(long, value_name = "SECS")]
    pub duration: Option<u64>,

    /// Stop after this many events.
    #[arg(long, value_name = "N")]
    pub limit: Option<u64>,

    /// OTLP/gRPC bind address. **Only** change this for tests: agents export to
    /// 4317, so another port yields an empty stream that looks healthy.
    #[arg(long, default_value_t = default_otlp_addr())]
    pub otlp_addr: SocketAddr,

    /// Hook datagram bind address. Same warning as `--otlp-addr`.
    #[arg(long, default_value_t = SocketAddrV4::new(Ipv4Addr::LOCALHOST, DEFAULT_HOOK_PORT))]
    pub hook_addr: SocketAddrV4,

    /// `~/.claude/projects`, or an override.
    #[arg(long, value_name = "DIR")]
    pub projects: Option<PathBuf>,

    /// Carry on when the hook port is already bound.
    ///
    /// Off by default: `AddrInUse` on that port means a second Polis is already
    /// receiving, and two daemons would each see roughly half the hook traffic
    /// while both looked healthy (ADR-0026).
    #[arg(long)]
    pub allow_second: bool,
}

/// `polis snapshot` — PRD §15 M1.
#[derive(Debug, clap::Args)]
pub struct SnapshotArgs {
    /// Where to write the city plan.
    #[arg(long, default_value = "polis.png")]
    pub out: PathBuf,

    /// Also write the road graph alone, junctions coloured by degree.
    ///
    /// The "is it a tree?" render: a network with cycles and no dangling ends
    /// is immediately, unarguably not a tree (PRD §7.2).
    #[arg(long, value_name = "PATH")]
    pub junctions: Option<PathBuf>,

    /// Also write the serialized layout, for a golden-file diff (PRD §16).
    #[arg(long, value_name = "PATH")]
    pub layout: Option<PathBuf>,

    /// Also write the PRD §10.3 band-validation render.
    ///
    /// The same base map with **simulated** M4 clouds and M5 agent and
    /// attention marks drawn on top, each inside the band §10.3 reserves for
    /// it. It answers one question and only that question: is a base map held
    /// in the bottom fifth of the contrast range still legible as context under
    /// the highlights it exists to make room for? Nothing in it is live data.
    ///
    /// The clouds are PRD §10.4's *"discrete iso-contour bands, 2–3 levels,
    /// never a continuous blur"* drawn literally — three nested contours and a
    /// hatch that tightens toward the core — because the wash that preceded them
    /// inked two thirds of its own footprint and moved the base median under it
    /// from `L 22` to `L 43`. Measure this render, not the palette: the median
    /// of the base map under a cloud has to match the median outside one.
    #[arg(long, value_name = "PATH")]
    pub bands: Option<PathBuf>,

    /// Draw a synthetic repository of this many files instead of the checkout.
    ///
    /// PRD §13.1 budgets cold start at 5 000 files and no fixture is that big;
    /// this is how the layout is exercised at the scale it has to hold at.
    #[arg(long, value_name = "FILES")]
    pub synthetic: Option<usize>,

    /// Seed for `--synthetic`. Fixed by default, so the fixture is a fixture.
    #[arg(long, default_value_t = 0xACCE_7107_0000_0001)]
    pub seed: u64,

    /// Draw PRD §9's import layer over the plan.
    ///
    /// Off by default: a street is a cross-district import relation, and the
    /// whole import graph drawn over the whole city at once is noise. The layer
    /// is for asking a question about one quarter, not for the city view.
    #[arg(long)]
    pub streets: bool,

    /// Output edge length in pixels.
    #[arg(long, default_value_t = 1600)]
    pub pixels: usize,

    /// Supersampling factor: the image is drawn this many times larger and box
    /// filtered down. Two is the useful setting.
    #[arg(long, default_value_t = 2)]
    pub supersample: usize,
}

/// `polis install-hooks` — PRD §4.2.
#[derive(Debug, clap::Args)]
pub struct InstallHooksArgs {
    /// Show what would be written without writing it.
    #[arg(long)]
    pub dry_run: bool,
    /// Write to the user settings file rather than the project's.
    #[arg(long)]
    pub user: bool,
    /// The `polis-hook` executable to register. Discovered next to `polis`,
    /// then in `target/release`, then on `PATH`, when not given.
    #[arg(long, value_name = "PATH")]
    pub hook_binary: Option<PathBuf>,
    /// Replace handler arrays Polis did not write. Without it, a conflict is
    /// reported and nothing is written.
    #[arg(long)]
    pub force: bool,
    /// Write this settings file instead of the project or user one.
    #[arg(long, value_name = "PATH")]
    pub settings: Option<PathBuf>,
}

/// `polis env` — PRD §4.1 as corrected by `docs/verified/otel-schema.md` §1.6.
#[derive(Debug, clap::Args)]
pub struct EnvArgs {
    /// Which shell to render for.
    #[arg(long, value_enum, default_value_t = ShellArg::default())]
    pub shell: ShellArg,
    /// The OTLP endpoint URL to point agents at.
    #[arg(long, default_value = "http://127.0.0.1:4317")]
    pub endpoint: String,
    /// Print only the variables, with no commentary — for `eval` and for
    /// writing a `.env` file.
    #[arg(long)]
    pub bare: bool,
}

/// `polis replay` — PRD §15 M2's foundation.
#[derive(Debug, clap::Args)]
pub struct ReplayArgs {
    /// A `.jsonl` transcript, or a `<session-id>` sidecar directory, in which
    /// case the whole forest is read: main transcript plus every subagent file.
    ///
    /// Optional, and that is the first-run experience: with no argument the
    /// window opens on the session picker, which lists this machine's own
    /// recordings most-recent first with their repository, duration and counts.
    /// Zero configuration — the operator picks one and watches it.
    pub transcript: Option<PathBuf>,

    /// Playback speed multiplier. Clamped to `polis_world::replay::SPEED_RANGE`.
    #[arg(long, default_value = "4.0")]
    pub speed: f32,

    /// Print the reconstructed event stream to stdout instead of opening the
    /// window.
    ///
    /// The M2 window is the product; this is the same read with no graphics,
    /// which is what a test, a pipe or a machine without a display wants.
    #[arg(long)]
    pub print: bool,

    /// One `RecordedEvent` JSON object per line, preceded by the header
    /// (ADR-0049). This is exactly `Replay::write_jsonl`. Implies `--print`.
    #[arg(long)]
    pub json: bool,

    /// Stop after this many events. Implies `--print`.
    #[arg(long, value_name = "N")]
    pub limit: Option<u64>,

    /// Write to a file instead of stdout. Implies `--print`.
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
}

impl ReplayArgs {
    /// Whether this invocation is the text one rather than the window.
    ///
    /// Any flag that only makes sense for a stream — `--json`, `--limit`,
    /// `--out` — selects it, so an existing pipeline keeps working without
    /// having to learn a new flag.
    pub fn is_text(&self) -> bool {
        self.print || self.json || self.limit.is_some() || self.out.is_some()
    }
}

/// A [`Channel`] as spelled on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ChannelArg {
    /// Channel A — OpenTelemetry.
    Otel,
    /// Channel B — hooks.
    Hook,
    /// Channel C — filesystem watch.
    Fs,
    /// Channel D — JSONL transcripts.
    Transcript,
    /// Polis's own health signals. Not an ingest channel; it can be filtered
    /// out of the output but never switched off.
    Control,
}

impl From<ChannelArg> for Channel {
    fn from(arg: ChannelArg) -> Self {
        match arg {
            ChannelArg::Otel => Self::Otel,
            ChannelArg::Hook => Self::Hook,
            ChannelArg::Fs => Self::Fs,
            ChannelArg::Transcript => Self::Transcript,
            ChannelArg::Control => Self::Control,
        }
    }
}

/// Which shell `polis env` renders for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum ShellArg {
    /// `export K=V`.
    Sh,
    /// `$env:K = "V"`.
    Powershell,
    /// Bare `K=V`, for a `.env` file or a `docker --env-file`.
    #[default]
    Env,
}

impl TailArgs {
    /// Channels to *display*. Empty `--source` means all of them.
    pub fn shown(&self) -> Vec<Channel> {
        if self.source.is_empty() {
            vec![
                Channel::Otel,
                Channel::Hook,
                Channel::Fs,
                Channel::Transcript,
                Channel::Control,
            ]
        } else {
            self.source.iter().copied().map(Channel::from).collect()
        }
    }

    /// Channels to *start*. Everything except `--without`.
    pub fn started(&self) -> ChannelSet {
        self.without
            .iter()
            .copied()
            .map(Channel::from)
            .fold(ChannelSet::ALL, ChannelSet::without)
    }
}

impl Cli {
    /// Resolves the repository root, defaulting to the current directory.
    ///
    /// Canonicalised, because [`polis_events::PathMapper`] strips a *textual*
    /// prefix: a mapper rooted at `.` would match nothing an agent reports, and
    /// the city would come out empty while looking healthy.
    pub fn repo_root(&self) -> std::io::Result<PathBuf> {
        let raw = match &self.repo {
            Some(path) => path.clone(),
            None => std::env::current_dir()?,
        };
        // `canonicalize` returns a `\\?\` verbatim path on Windows. `PathMapper`
        // handles it, but every message printed from here reads better without,
        // so it is stripped for display and kept for matching only if the strip
        // would change the meaning — it cannot, for a plain drive path.
        let canonical = raw.canonicalize().unwrap_or(raw);
        Ok(strip_verbatim(canonical))
    }
}

/// Removes Windows' `\\?\` verbatim prefix when it is safe to.
///
/// Safe exactly when what follows is a plain `X:\…` drive path; a `\\?\UNC\…`
/// path keeps its prefix, because dropping it changes which share it names.
pub fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        let mut chars = rest.chars();
        let drive = chars.next();
        if drive.is_some_and(|c| c.is_ascii_alphabetic()) && chars.next() == Some(':') {
            return PathBuf::from(rest);
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_filters_the_output_and_never_silently_stops_a_channel() {
        let cli = Cli::parse_from(["polis", "tail", "--source", "hook", "--source", "fs"]);
        let Some(Command::Tail(args)) = cli.command else {
            panic!("expected tail")
        };
        assert_eq!(args.shown(), vec![Channel::Hook, Channel::Fs]);
        // Filtering the display must not stop the other channels: a counters
        // line that reported only what was shown would make a drop invisible.
        assert_eq!(args.started(), ChannelSet::ALL);

        let cli = Cli::parse_from(["polis", "tail", "--without", "otel"]);
        let Some(Command::Tail(args)) = cli.command else {
            panic!("expected tail")
        };
        assert!(!args.started().contains(Channel::Otel));
        assert!(args.started().contains(Channel::Hook));
        assert_eq!(args.shown().len(), 5, "no --source shows everything");
    }

    #[test]
    fn the_defaults_are_the_only_ports_agents_are_configured_to_use() {
        let cli = Cli::parse_from(["polis", "tail"]);
        let Some(Command::Tail(args)) = cli.command else {
            panic!("expected tail")
        };
        assert_eq!(args.otlp_addr.to_string(), polis_ingest::DEFAULT_OTLP_ADDR);
        assert_eq!(args.hook_addr.port(), DEFAULT_HOOK_PORT);
        assert!(!args.allow_second, "a second daemon is refused by default");
        assert_eq!(args.counters_every, 5);
    }

    #[test]
    fn every_subcommand_parses() {
        for argv in [
            vec!["polis", "install-hooks", "--dry-run"],
            vec!["polis", "install-hooks", "--user", "--force"],
            vec!["polis", "env", "--shell", "powershell"],
            vec!["polis", "replay", "session.jsonl", "--json"],
            vec!["polis", "doctor"],
            vec!["polis", "-C", ".", "tail", "--json", "--duration", "3"],
        ] {
            Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }
    }

    #[test]
    fn a_verbatim_prefix_is_stripped_only_for_a_plain_drive_path() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\coding\agentolis")),
            PathBuf::from(r"C:\coding\agentolis")
        );
        // A UNC share keeps its prefix: dropping it names a different thing.
        let unc = PathBuf::from(r"\\?\UNC\server\share\repo");
        assert_eq!(strip_verbatim(unc.clone()), unc);
        assert_eq!(
            strip_verbatim(PathBuf::from("/home/op/repo")),
            PathBuf::from("/home/op/repo")
        );
    }
}
