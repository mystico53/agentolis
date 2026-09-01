//! The `polis` binary (PRD §14).
//!
//! Deliberately thin: it parses the command line and hands off. winit owns the
//! main thread from `polis_app::run` onward, which is why there is no
//! `#[tokio::main]` here or anywhere else in the workspace.

use clap::Parser as _;
use polis_app::cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => todo!("PRD §13 — open the window"),
        Some(Command::Tail { .. }) => todo!("PRD §15 M0 — print the normalized event stream"),
        Some(Command::InstallHooks { .. }) => todo!("PRD §4.2 — write the 19 registrations"),
        Some(Command::Replay { .. }) => todo!("PRD §15 M2 — animate a recorded session"),
        Some(Command::Snapshot { .. }) => todo!("PRD §15 M1 — deterministic city to PNG"),
        Some(Command::Doctor) => todo!("PRD §4 — report environment and channel health"),
    }
}
