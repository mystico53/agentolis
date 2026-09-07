//! The `polis` binary (PRD §14).
//!
//! Deliberately thin: it parses the command line and hands off. winit owns the
//! main thread from `polis_app::run` onward, which is why there is no
//! `#[tokio::main]` here or anywhere else in the workspace.
//!
//! # It must never vanish
//!
//! Double-clicking `polis.exe` on Windows opens a console, runs the program with
//! no arguments and closes the console the instant it returns. Before there was
//! a window that read as a crash, and it was the first thing most people would
//! ever see. Two rules fix it, and they are both here rather than in a launcher
//! script, because the operator can double-click the binary directly:
//!
//! 1. **No arguments opens the window.** [`polis_app::setup::first_run`] always
//!    ends in a window or in an explanation of why there is nothing to draw.
//! 2. **A failure waits.** When there were no arguments and there is a terminal
//!    on both ends, the error is printed and then the process waits for Enter,
//!    so the message can be read. See
//!    [`polis_app::setup::should_pause`] for why the test is that and not
//!    "do I own this console", which cannot be asked without `unsafe`.

use clap::Parser as _;
use polis_app::cli::{Cli, Command};
use polis_app::{commands, run, setup, watch};

fn main() {
    // Captured before `parse`, which can exit the process on `--help`: this is
    // the "was this double-clicked?" signal, and it has to survive that path.
    let bare = std::env::args_os().len() <= 1;
    match dispatch() {
        Ok(code) => {
            if code != 0 {
                std::process::exit(code);
            }
        }
        Err(error) => {
            eprintln!("polis: {error:#}");
            if setup::should_pause(!bare) {
                setup::pause();
            }
            std::process::exit(1);
        }
    }
}

/// Everything `main` does, in a form that can return an error.
///
/// Returns the exit code, which for `polis run` is the agent's own — a script
/// that wraps `claude` in `polis run` has to keep behaving like the script it
/// was.
fn dispatch() -> anyhow::Result<i32> {
    let cli = Cli::parse();
    match &cli.command {
        // PRD §15: `polis` with no arguments is the whole product for someone
        // who has not read anything. It detects what is on the machine, explains
        // the map in one screen, and opens either the session picker (first run,
        // or nothing here to map) or this repository as a city.
        None => setup::first_run(&cli).map(|()| 0),

        // PRD §2 keeps one window to one repository; this is where the operator
        // chooses which, from a list that says where agents are actually
        // working. Bare `polis` opens the same screen.
        Some(Command::Home) => {
            let repo = cli.repo_root()?;
            let here = polis_app::repos::checkout_containing(&repo);
            let config = polis_app::config::Config {
                repo_root: repo,
                ..polis_app::config::Config::default()
            };
            polis_app::launch(config, polis_app::Mode::Home { here }).map(|()| 0)
        }
        Some(Command::Run(args)) => run::run(&cli, args),
        Some(Command::Map) => {
            let config = polis_app::config::Config {
                repo_root: cli.repo_root()?,
                ..polis_app::config::Config::default()
            };
            polis_app::run(config).map(|()| 0)
        }
        // PRD §15 M7: the map, and agents in panes beside it. The agents
        // belong to `polis-sessiond`, so this window can be closed and reopened
        // around them.
        Some(Command::Work(args)) => {
            let repo = cli.repo_root()?;
            let (program, agent_args) = args.agent();
            let config = polis_app::config::Config {
                repo_root: repo.clone(),
                ..polis_app::config::Config::default()
            };
            polis_app::launch(
                config,
                polis_app::Mode::Work {
                    repo,
                    panes: args.panes.max(1),
                    program,
                    args: agent_args,
                },
            )
            .map(|()| 0)
        }
        // PRD §15 M3, and the front door: every agent working in this
        // repository, live, with no configuration at all. The picker `watch`
        // used to open is `polis replay` with no argument.
        Some(Command::Watch(args)) => watch::watch(&cli, args),
        Some(Command::Connect(args)) => setup::connect(&cli, args).map(|()| 0),
        Some(Command::Tail(args)) => commands::tail(&cli, args).map(|()| 0),
        Some(Command::InstallHooks(args)) => commands::install_hooks(&cli, args).map(|()| 0),
        Some(Command::Env(args)) => commands::env(args).map(|()| 0),
        Some(Command::Replay(args)) => commands::replay(&cli, args).map(|()| 0),
        Some(Command::Snapshot(args)) => polis_app::snapshot::snapshot(&cli, args).map(|()| 0),
        Some(Command::Doctor(args)) => setup::doctor(&cli, args).map(|()| 0),
    }
}
