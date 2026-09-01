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
        // PRD §15 M0 is the milestone this build gates on, and it is explicitly
        // "no graphics". Opening a window is M1/M3 work: `polis_app::run` is
        // still a `todo!()` and calling it would panic, so this says what the
        // binary can do today instead of aborting with a backtrace.
        None => Err(anyhow::anyhow!(
            "the window is not built yet (PRD §15 M1/M3).\n\n\
             What works today:\n  \
             polis tail            print the normalized event stream (PRD §15 M0)\n  \
             polis install-hooks   write the .claude/settings.json hooks block\n  \
             polis env             print the telemetry block an agent needs\n  \
             polis replay <file>   read one transcript offline\n  \
             polis doctor          report the environment and channel health"
        )),
        Some(Command::Tail(args)) => commands::tail(&cli, args),
        Some(Command::InstallHooks(args)) => commands::install_hooks(&cli, args),
        Some(Command::Env(args)) => commands::env(args),
        Some(Command::Replay(args)) => commands::replay(&cli, args),
        Some(Command::Snapshot { out }) => Err(anyhow::anyhow!(
            "`polis snapshot` needs the layout and the renderer (PRD §15 M1); \
             nothing would be written to {}",
            out.display()
        )),
        Some(Command::Doctor) => commands::doctor(&cli),
    }
}
