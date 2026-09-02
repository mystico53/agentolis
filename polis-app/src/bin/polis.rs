//! The `polis` binary (PRD §14).
//!
//! Deliberately thin: it parses the command line and hands off. winit owns the
//! main thread from `polis_app::run` onward, which is why there is no
//! `#[tokio::main]` here or anywhere else in the workspace.

use clap::Parser as _;
use polis_app::cli::{Cli, Command};
use polis_app::commands;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        // PRD §15 M1: "Generate a city from git history and render it to a
        // window". `polis` with no arguments **is** that window, over the
        // checkout the operator is standing in.
        None => {
            let config = polis_app::config::Config {
                repo_root: cli.repo_root()?,
                ..polis_app::config::Config::default()
            };
            polis_app::run(config)
        }
        Some(Command::Tail(args)) => commands::tail(&cli, args),
        Some(Command::InstallHooks(args)) => commands::install_hooks(&cli, args),
        Some(Command::Env(args)) => commands::env(args),
        Some(Command::Replay(args)) => commands::replay(&cli, args),
        Some(Command::Snapshot(args)) => polis_app::snapshot::snapshot(&cli, args),
        Some(Command::Doctor) => commands::doctor(&cli),
    }
}
