//! The command line (PRD §4.2, §15).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
    Tail {
        /// Only show these channels.
        #[arg(long)]
        channel: Vec<String>,
    },

    /// Write the `.claude/settings.json` hooks block (PRD §4.2).
    ///
    /// Merges key-by-key and warns before replacing any array Polis did not
    /// write. Registers 19 events in **exec form** — never a shell command,
    /// which costs 6× on Git Bash and 25× on PowerShell per event — and
    /// deliberately does **not** register `WorktreeCreate`, which would break
    /// every worktree on the machine.
    InstallHooks {
        /// Show what would be written without writing it.
        #[arg(long)]
        dry_run: bool,
        /// Write to the user settings file rather than the project's.
        #[arg(long)]
        user: bool,
    },

    /// Animate a recorded session over the city (PRD §15 M2).
    ///
    /// The fastest iteration loop the project has, and it ships independently as
    /// a PR-summary or standup artifact.
    Replay {
        /// A `.jsonl` transcript.
        transcript: PathBuf,
        /// Playback speed multiplier.
        #[arg(long, default_value = "1.0")]
        speed: f32,
    },

    /// Generate the city and write it to a PNG without opening a window.
    ///
    /// PRD §15 M1's gate: byte-identical layout across two runs and two machines.
    Snapshot {
        /// Where to write the image.
        #[arg(long, default_value = "polis.png")]
        out: PathBuf,
    },

    /// Report environment, channel health and the injected agent env block.
    ///
    /// Prints the environment variables an operator must set for an agent Polis
    /// did not launch — the common case, since operators start `claude`
    /// themselves and the endpoint env var only reaches Polis's own children.
    Doctor,
}

impl Cli {
    /// Resolves the repository root, defaulting to the current directory.
    pub fn repo_root(&self) -> std::io::Result<PathBuf> {
        todo!("PRD §2 — one repository, one city")
    }
}
