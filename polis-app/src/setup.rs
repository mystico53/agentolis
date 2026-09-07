//! The front door: what Polis does with no configuration at all (PRD §15).
//!
//! Everything else in this crate assumes an operator who already knows that
//! agent telemetry arrives over OTLP on 4317, that hooks live in a
//! `.claude/settings.json` block, and that a transcript is a `.jsonl` file under
//! `~/.claude/projects/<munged-cwd>/`. None of that is knowledge a first-time
//! user has, and none of it is knowledge they should need.
//!
//! So this module owns four things, and they are all onboarding:
//!
//! * [`detect`] — what is actually on this machine: a repository, Claude Code,
//!   a transcripts directory, past sessions, installed hooks, a built
//!   `polis-hook`. One struct, printed as one screen.
//! * [`first_run`] — what bare `polis` does: say what is on this machine in one
//!   screen, then open the **repository launcher** ([`crate::repos`]), which
//!   lists every checkout with an agent working in it right now and watches the
//!   one that is picked. It used to guess instead — the session picker on a
//!   first run, a map of the current directory afterwards — and both guesses
//!   were about the folder the terminal happened to be standing in rather than
//!   the repository the work is in. `polis replay` is still the session picker.
//! * [`connect`] — hook installation with explicit consent: the exact file, a
//!   line diff against what is there now, a backup, a confirmation, and an
//!   uninstall that is tested to put the file back.
//! * [`doctor`] — every problem with the specific command that fixes it, and
//!   `--fix` to apply the safe ones.
//!
//! # The two hook safety rules are enforced here as well as in `polis-ingest`
//!
//! [`audit`] re-checks the settings JSON *about to be written* for the two rules
//! `docs/verified/hooks-schema.md` §9.1 and §9.3 establish — no
//! `WorktreeCreate`, and `PreToolUse` narrowed to `^(Edit|Write|NotebookEdit)$`.
//! `polis-ingest` cannot construct a block that breaks them, and this checks
//! anyway: a `WorktreeCreate` handler replaces git's worktree creation and then
//! fails it, which breaks every worktree on the machine, and that is not a class
//! of bug worth trusting one layer with.

use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::net::{SocketAddr, SocketAddrV4, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Context as _;
use polis_events::DEFAULT_HOOK_PORT;
use polis_ingest::env::{self as hookenv, HookInstall};
use serde_json::{Map, Value};

use crate::cli::{Cli, ConnectArgs, DoctorArgs};

/// How long [`probe_version`] waits for `<agent> --version` before giving up.
///
/// A version probe is a convenience, not a check: an agent that is slow to
/// answer must not make `polis doctor` look hung.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// What is on this machine
// ---------------------------------------------------------------------------

/// Everything the first-run screen and `polis doctor` both need to know.
///
/// Gathered once, cheaply: the session scan is headers-only (67 ms over 187
/// sessions on the reference machine), and nothing here spawns a process except
/// the optional version probe, which is timed out.
#[derive(Debug, Clone)]
pub struct Detected {
    /// The repository root Polis would map.
    pub repo: PathBuf,
    /// Whether that root is actually a git checkout. Without it there is no
    /// growth order and therefore no city (PRD §7.1).
    pub is_git: bool,
    /// `~/.claude/projects`, when there is a home directory to derive it from.
    pub projects_dir: Option<PathBuf>,
    /// Whether that directory exists.
    pub projects_exists: bool,
    /// Sessions found there.
    pub sessions: usize,
    /// Of those, how many still have their repository on disk.
    pub replayable: usize,
    /// Distinct repositories those sessions came from.
    pub repositories: usize,
    /// The most recent replayable session's transcript, for "watch it back".
    pub newest: Option<PathBuf>,
    /// Sessions recorded against [`Detected::repo`] itself.
    pub sessions_here: usize,
    /// The `claude` executable, if one is on `PATH`.
    pub claude: Option<PathBuf>,
    /// The `polis-hook` executable, if one has been built.
    pub hook_binary: Option<PathBuf>,
    /// Polis hook registrations in `<repo>/.claude/settings.json`.
    pub hooks_project: usize,
    /// Polis hook registrations in `~/.claude/settings.json`.
    pub hooks_user: usize,
    /// Whether the settings file mentions `WorktreeCreate` at all — a hook there
    /// breaks every worktree on the machine (`hooks-schema.md` §9.1).
    pub worktree_hook: bool,
}

impl Detected {
    /// Whether there is anything at all to show right now.
    pub fn has_something_to_show(&self) -> bool {
        self.is_git || self.replayable > 0
    }

    /// Whether hooks are registered anywhere that would apply to this repo.
    pub fn hooks_installed(&self) -> bool {
        self.hooks_project > 0 || self.hooks_user > 0
    }
}

/// Looks at the machine. Never fails; every unknown is a field, not an error.
pub fn detect(repo: &Path) -> Detected {
    let projects_dir = polis_ingest::default_claude_projects_dir();
    let projects_exists = projects_dir.as_ref().is_some_and(|d| d.is_dir());

    let mut sessions = 0;
    let mut replayable = 0;
    let mut repositories = 0;
    let mut newest = None;
    let mut sessions_here = 0;
    if let Some(dir) = projects_dir.as_ref().filter(|_| projects_exists) {
        // Headers only. The full byte scan is 493 ms cold and this screen is on
        // the path to the first frame.
        if let Ok(index) = polis_world::sessions::SessionIndex::scan_with(
            dir,
            &polis_world::sessions::IndexOptions::quick(),
        ) {
            sessions = index.len();
            repositories = index.repositories().len();
            for session in &index.sessions {
                if session.is_replayable() {
                    replayable += 1;
                    if newest.is_none() {
                        newest = Some(session.transcript.clone());
                    }
                }
                if session.repo.as_deref() == Some(repo) {
                    sessions_here += 1;
                }
            }
        }
    }

    let project_settings = repo.join(".claude").join("settings.json");
    let user_settings = user_settings_path();
    let (hooks_project, worktree_project) = count_polis_hooks(&project_settings);
    let (hooks_user, worktree_user) = user_settings
        .as_deref()
        .map_or((0, false), count_polis_hooks);

    Detected {
        repo: repo.to_path_buf(),
        is_git: repo.join(".git").exists(),
        projects_dir,
        projects_exists,
        sessions,
        replayable,
        repositories,
        newest,
        sessions_here,
        claude: which("claude"),
        hook_binary: hook_binary(repo),
        hooks_project,
        hooks_user,
        worktree_hook: worktree_project || worktree_user,
    }
}

/// `~/.claude/settings.json`.
pub fn user_settings_path() -> Option<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .or_else(|| std::env::var_os(if cfg!(windows) { "HOME" } else { "USERPROFILE" }))?;
    Some(PathBuf::from(home).join(".claude").join("settings.json"))
}

/// A built `polis-hook`, next to this binary or in one of the build profiles.
///
/// On Windows this is only ever an `.exe`: hook registration is exec form, and a
/// `.cmd` shim cannot be spawned without a shell (`hooks-schema.md` §5.3).
pub fn hook_binary(repo: &Path) -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        "polis-hook.exe"
    } else {
        "polis-hook"
    };
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        candidates.push(dir.join(exe));
    }
    for profile in ["hook", "release", "debug"] {
        candidates.push(repo.join("target").join(profile).join(exe));
    }
    candidates
        .into_iter()
        .find(|c| c.is_file())
        .map(|c| crate::cli::strip_verbatim(c.canonicalize().unwrap_or(c.clone())))
}

/// Reads a JSON file, tolerating a leading UTF-8 byte-order mark.
///
/// `None` when the file is not there; `Some(Err)` when it is there and does not
/// parse — which must never be treated as "start from scratch", because that
/// silently deletes an operator's settings. The BOM is stripped because Windows
/// editors write one and it is not a reason to call a file corrupt on **read**;
/// writing into such a file is still refused, with [`diagnose_unparseable`]
/// saying why.
fn read_json(path: &Path) -> Option<Result<Value, serde_json::Error>> {
    let bytes = std::fs::read(path).ok()?;
    let body = bytes.strip_prefix(UTF8_BOM).unwrap_or(&bytes);
    let text = String::from_utf8_lossy(body);
    if text.trim().is_empty() {
        return Some(Ok(Value::Object(Map::new())));
    }
    Some(serde_json::from_str(&text))
}

/// How many hook events in `path` run `polis-hook`, and whether the file
/// registers `WorktreeCreate` at all.
fn count_polis_hooks(path: &Path) -> (usize, bool) {
    let Some(Ok(value)) = read_json(path) else {
        return (0, false);
    };
    let Some(hooks) = value.get("hooks").and_then(Value::as_object) else {
        return (0, false);
    };
    let ours = hooks.values().filter(|g| group_is_polis(g)).count();
    (ours, hooks.contains_key("WorktreeCreate"))
}

/// Whether a `hooks.<Event>` array contains a `polis-hook` handler.
fn group_is_polis(groups: &Value) -> bool {
    groups
        .as_array()
        .is_some_and(|groups| groups.iter().any(handler_is_polis))
}

/// Whether one `{ matcher?, hooks: [...] }` group runs `polis-hook`.
fn handler_is_polis(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|handlers| {
            handlers.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains("polis-hook"))
            })
        })
}

// ---------------------------------------------------------------------------
// The one screen
// ---------------------------------------------------------------------------

/// The whole explanation of what Polis is, in one screen (PRD §10, §12).
///
/// Deliberately shorter than a page and free of every word a first-time user has
/// no reason to know: no "OTLP", no "territory inference", no milestone numbers.
/// Four sentences about the picture, then what was found on this machine.
pub fn orientation(out: &mut impl Write, d: &Detected) -> io::Result<()> {
    writeln!(out)?;
    writeln!(out, "  P O L I S")?;
    writeln!(out, "  your coding agents, drawn as a city seen from above")?;
    writeln!(out)?;
    writeln!(out, "  What you are looking at")?;
    // The same strings the window's first-run overlay and `h` sheet render, so
    // the four ways into the map cannot describe it differently.
    for line in crate::explain::MAP {
        writeln!(out, "    {line}")?;
    }
    writeln!(
        out,
        "    An agent shows up as a cloud over where it is working, and leaves"
    )?;
    writeln!(
        out,
        "    a trail behind it. Click a building to open the file."
    )?;
    writeln!(out)?;
    writeln!(out, "  On this machine")?;
    let git = if d.is_git {
        "git checkout"
    } else {
        "NOT a git repository — no history, so no city"
    };
    writeln!(out, "    repo          {} ({git})", d.repo.display())?;
    match (&d.projects_dir, d.projects_exists) {
        (Some(dir), true) => writeln!(
            out,
            "    sessions      {} past sessions, {} replayable, {} repositories",
            d.sessions, d.replayable, d.repositories
        )
        .and_then(|()| writeln!(out, "                  in {}", dir.display()))?,
        (Some(dir), false) => writeln!(
            out,
            "    sessions      none yet ({} does not exist)",
            dir.display()
        )?,
        (None, _) => writeln!(out, "    sessions      no home directory to look in")?,
    }
    match &d.claude {
        Some(path) => writeln!(out, "    Claude Code   {}", path.display())?,
        None => writeln!(out, "    Claude Code   not on PATH")?,
    }
    writeln!(out)?;
    Ok(())
}

/// The three commands, printed wherever the operator might need them next.
fn next_steps(out: &mut impl Write, d: &Detected) -> io::Result<()> {
    writeln!(out, "  What you can do")?;
    writeln!(
        out,
        "    polis watch            pick one of your past sessions and watch it"
    )?;
    writeln!(
        out,
        "    polis map              this repository as a city, right now"
    )?;
    writeln!(
        out,
        "    polis run -- claude    start an agent with the map already watching"
    )?;
    if !d.hooks_installed() {
        writeln!(
            out,
            "    polis connect          let Polis see agents you start yourself"
        )?;
    }
    writeln!(
        out,
        "    polis doctor           what is wrong, and how to fix it"
    )?;
    writeln!(out)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Bare `polis`
// ---------------------------------------------------------------------------

/// What bare `polis` does (PRD §15 M1/M2).
///
/// The rule, in one line: **on a machine that has never run Polis, open the
/// session picker; afterwards, map the checkout you are standing in.**
///
/// That ordering is the whole onboarding argument. A first-time user has no
/// uncommitted work, so the city they would get is flat and nothing moves in
/// it — while the same machine already holds hundreds of real recorded sessions
/// that animate immediately with nothing to configure. So the first run spends
/// its one chance on the thing that moves, says so on screen, and prints the
/// command that gets the map back.
pub fn first_run(cli: &Cli) -> anyhow::Result<()> {
    let repo = cli.repo_root().context("resolving the repository root")?;
    let detected = detect(&repo);
    let first = !seen_before();

    let mut out = io::stdout().lock();
    orientation(&mut out, &detected)?;

    if !detected.has_something_to_show() {
        writeln!(
            out,
            "  There is nothing to draw yet: this folder is not a git repository\n  \
             and no agent sessions were found on this machine.\n"
        )?;
        writeln!(
            out,
            "  Do one of these, then run polis again:\n    \
             cd into a git repository and run  polis\n    \
             run  polis run -- claude  to start an agent here\n"
        )?;
        out.flush()?;
        return Ok(());
    }

    // The launcher, always. Which repository to watch is a question the shell's
    // working directory answers badly — an operator with agents in three
    // checkouts is standing in at most one of them — and the answer is already
    // on disk: every session on this machine says which directory it is working
    // in. So bare `polis` asks, with the live count against each row, rather
    // than guessing between a map of wherever the terminal happens to be and a
    // picker of what has already finished.
    let here = crate::repos::checkout_containing(&repo);
    if first {
        writeln!(
            out,
            "  First run, so Polis is opening the repository launcher: it lists every\n  \
             checkout with an agent working in it right now, and every one this machine\n  \
             has run them in before. Pick one and it is watched live, with nothing to\n  \
             set up.\n"
        )?;
    } else if let Some(here) = &here {
        writeln!(
            out,
            "  Opening the repository launcher. {} is at the top of it; pick it\n  \
             or any other checkout agents are working in.\n",
            here.display()
        )?;
    } else {
        writeln!(
            out,
            "  This folder is not a git repository, so the launcher opens without it:\n  \
             pick any checkout agents are working in.\n"
        )?;
    }
    next_steps(&mut out, &detected)?;
    // Every command just printed begins with the word `polis`, and on a fresh
    // checkout that word is not on PATH — the first-run review's first blocker.
    // Saying so here, next to the commands it applies to, is cheaper than the
    // operator discovering it one command at a time.
    if let Some(note) = path_note() {
        write!(out, "{note}")?;
    }
    out.flush()?;
    drop(out);

    remember_seen();
    let config = crate::config::Config {
        repo_root: repo.clone(),
        ..crate::config::Config::default()
    };
    crate::launch(config, crate::Mode::Home { here })
}

/// The warning that the commands just printed cannot be typed as written.
///
/// `None` when `polis` on `PATH` is this binary, which is the state the guide
/// assumes and the state [`path_fix`] produces.
fn path_note() -> Option<String> {
    let check = check_on_path();
    if check.status == Status::Ok {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let exe = crate::cli::strip_verbatim(exe.canonicalize().unwrap_or(exe));
    let mut note = format!(
        "  Those commands start with the word `polis`, and it is not on PATH\n  \
         yet — for now, spell it out:\n\n    {} watch\n\n",
        exe.display()
    );
    for line in fix_lines(check.fix.as_deref().unwrap_or_default()) {
        note.push_str("  ");
        note.push_str(&line);
        note.push('\n');
    }
    note.push('\n');
    Some(note)
}

/// The marker that says this machine has run Polis before.
fn seen_marker() -> Option<PathBuf> {
    crate::config::Config::default_state_dir().map(|dir| dir.join("first-run"))
}

/// Whether Polis has been run on this machine before.
fn seen_before() -> bool {
    seen_marker().is_some_and(|p| p.exists())
}

/// Records that it has. Failure is silent: a read-only state directory means one
/// extra picker, which is not worth an error message on the way to a window.
fn remember_seen() {
    let Some(path) = seen_marker() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, "polis has been run on this machine\n");
}

// ---------------------------------------------------------------------------
// polis connect — hooks, with consent
// ---------------------------------------------------------------------------

/// Installs or removes the hook registrations, with explicit consent (PRD §4.2).
///
/// `polis install-hooks` already merges safely; what it does not do is *ask*.
/// This shows the exact file, a line diff of what changes in it, the backup it
/// will take, and then waits for a yes. `--uninstall` reverses it.
pub fn connect(cli: &Cli, args: &ConnectArgs) -> anyhow::Result<()> {
    let repo = cli.repo_root().context("resolving the repository root")?;
    let settings = match (&args.settings, args.user) {
        (Some(path), _) => path.clone(),
        (None, true) => {
            user_settings_path().context("no home directory to place ~/.claude/settings.json in")?
        }
        (None, false) => repo.join(".claude").join("settings.json"),
    };
    let mut out = io::stdout().lock();
    if args.uninstall {
        return disconnect(&mut out, &settings, args.yes);
    }

    let hook = match &args.hook_binary {
        Some(path) => path.clone(),
        None => hook_binary(&repo).unwrap_or_else(|| {
            PathBuf::from(if cfg!(windows) {
                "polis-hook.exe"
            } else {
                "polis-hook"
            })
        }),
    };

    let exists = settings.exists();
    let scope = if args.settings.is_some() {
        Scope::Explicit
    } else if args.user {
        Scope::User
    } else {
        Scope::Project
    };
    explain_connect(&mut out, &settings, &hook, exists, scope)?;

    let plan = hookenv::plan_hooks(&settings, &hook);
    // Blocked first, and only then the audit. The other order was wrong and this
    // is how it showed: a settings file Polis refuses to parse leaves
    // `settings_json` empty, `audit` failed on the empty string, and the
    // operator got "the planned settings file is not JSON: EOF while parsing"
    // about a file that looked perfectly fine in their editor.
    if plan.blocked {
        for note in &plan.notes {
            writeln!(out, "  {note}")?;
        }
        if let Some(hint) = diagnose_unparseable(&settings) {
            writeln!(out, "  {hint}")?;
        }
        out.flush()?;
        anyhow::bail!(
            "refusing to touch {}; fix or move it and run polis connect again",
            settings.display()
        );
    }
    audit(&plan.settings_json).context("refusing to write a hooks block that is not safe")?;

    let before = std::fs::read_to_string(&settings).unwrap_or_default();
    let after = format!("{}\n", plan.settings_json);
    let changes = diff(&before, &after);
    if changes.iter().all(|line| matches!(line, DiffLine::Same(_))) {
        writeln!(out, "  Already connected — that file is exactly right.")?;
        writeln!(out, "  Undo with:  polis connect --uninstall")?;
        writeln!(out)?;
        out.flush()?;
        return Ok(());
    }
    show_change(&mut out, &plan, &changes, exists)?;

    if plan.has_conflicts() && !args.force {
        writeln!(
            out,
            "  {} already registers handlers Polis did not write:\n    {}",
            settings.display(),
            plan.conflicts.join(", ")
        )?;
        writeln!(
            out,
            "  Nothing was written. Re-run with --force to replace them."
        )?;
        out.flush()?;
        anyhow::bail!("conflicting hook registrations");
    }
    if exists {
        writeln!(
            out,
            "  The current file is copied to\n    {}\n  before anything is written.",
            hookenv::backup_path(&settings).display()
        )?;
        writeln!(out)?;
    }
    if args.dry_run {
        writeln!(out, "  --dry-run. The file it would write, in full:")?;
        writeln!(out)?;
        for line in after.lines() {
            writeln!(out, "    {line}")?;
        }
        writeln!(out)?;
        writeln!(out, "  Nothing was written.")?;
        out.flush()?;
        return Ok(());
    }

    out.flush()?;
    drop(out);
    if !confirm("  Write it?", args.yes)? {
        let mut out = io::stdout().lock();
        writeln!(out, "  Nothing was written.")?;
        out.flush()?;
        return Ok(());
    }
    write_hooks(&settings, &hook)
}

/// The write itself, and what to say afterwards.
fn write_hooks(settings: &Path, hook: &Path) -> anyhow::Result<()> {
    let done = hookenv::install_hooks(settings, hook);
    let mut out = io::stdout().lock();
    if !done.written {
        for note in &done.notes {
            writeln!(out, "  {note}")?;
        }
        out.flush()?;
        anyhow::bail!("nothing was written");
    }
    writeln!(out)?;
    writeln!(
        out,
        "  Connected. {} events registered.",
        done.added.len() + done.unchanged.len()
    )?;
    if let Some(backup) = &done.backup {
        writeln!(out, "  Previous file saved as {}", backup.display())?;
    }
    writeln!(out, "  Undo with:  polis connect --uninstall")?;
    writeln!(out)?;
    // The single most common "it did not work" report, and it is not a failure:
    // in an interactive session Claude Code runs no settings-file hook at all —
    // including from ~/.claude/settings.json — until the workspace trust dialog
    // is accepted for the folder (hooks-schema.md §5.6).
    writeln!(
        out,
        "  One thing that looks like a failure and is not: in an interactive\n  \
         session Claude Code runs no settings-file hook until you accept the\n  \
         workspace trust dialog for this folder. Accept it once and hooks fire."
    )?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

/// What `polis connect` is about to do, in words, before it shows the diff.
///
/// The two "deliberately does NOT" lines are not decoration: an operator who has
/// been told what a tool will not do can believe the rest of the screen, and the
/// `WorktreeCreate` one in particular is the difference between a hook system
/// that is safe to install and one that breaks `git worktree` machine-wide.
/// Which settings file `polis connect` is about to write, in the operator's
/// terms rather than clap's.
///
/// It matters and it was invisible: connect writes **this repository's**
/// `.claude/settings.json` by default, so connecting in repo A and then starting
/// `claude` in repo B sees nothing, and no screen said the choice existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `<repo>/.claude/settings.json` — this repository only. The default.
    Project,
    /// `~/.claude/settings.json` — every repository on this machine.
    User,
    /// A path the operator named with `--settings`.
    Explicit,
}

impl Scope {
    /// Which repositories this covers, and the command that gets the other
    /// answer — separately, so the command is never wrapped mid-line.
    pub fn describe(self) -> (&'static str, Option<&'static str>) {
        match self {
            Self::Project => (
                "That is this repository's own settings file: it covers agents \
                 started in this checkout, and no others. To cover every \
                 repository on this machine instead:",
                Some("polis connect --user"),
            ),
            Self::User => (
                "That is your user settings file: it covers agents started in \
                 every repository on this machine. To cover this checkout \
                 alone instead:",
                Some("polis connect"),
            ),
            Self::Explicit => ("That is the file you named with --settings.", None),
        }
    }
}

fn explain_connect(
    out: &mut impl Write,
    settings: &Path,
    hook: &Path,
    exists: bool,
    scope: Scope,
) -> io::Result<()> {
    writeln!(out)?;
    writeln!(out, "  Connect Polis to Claude Code")?;
    writeln!(out, "  ----------------------------")?;
    writeln!(out)?;
    writeln!(out, "  This writes a hooks block into")?;
    writeln!(
        out,
        "    {}{}",
        settings.display(),
        if exists { "" } else { "   (will be created)" }
    )?;
    writeln!(out)?;
    writeln!(
        out,
        "  so that Claude Code tells Polis when it starts a session, edits a\n  \
         file, finishes a tool call or stops to ask you something."
    )?;
    writeln!(out)?;
    // Which repositories this covers. It is the one thing about `connect` that
    // is invisible and wrong-by-default for half the people who run it: the
    // project file is written unless `--user` is passed, and nothing said so.
    let (prose, command) = scope.describe();
    for line in wrap(prose, 70) {
        writeln!(out, "  {line}")?;
    }
    if let Some(command) = command {
        writeln!(out, "    {command}")?;
    }
    writeln!(out)?;
    writeln!(out, "  It registers 19 events, each of which runs")?;
    writeln!(out, "    {} --event <name>", hook.display())?;
    if !hook.exists() {
        writeln!(
            out,
            "  WARNING: that file does not exist yet. Build it first with"
        )?;
        writeln!(out, "    cargo build --profile hook -p polis-hook")?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "  Two things it deliberately does NOT do:\n    \
         PreToolUse is narrowed to {} so it fires on\n      \
         file edits only, not on every tool call.\n    \
         WorktreeCreate is not registered at all. A hook there replaces git's\n      \
         worktree creation and then fails it, which would break every\n      \
         worktree on this machine.",
        hookenv::PRE_TOOL_USE_MATCHER
    )?;
    writeln!(out)?;
    Ok(())
}

/// The middle of the consent screen: what is registered, and what changes.
///
/// The event list is the change when the file is being created from nothing:
/// the raw diff is then 291 lines of generated JSON, which pushes the two safety
/// rules off the top of the screen and asks for consent to something nobody
/// read. So the line diff appears when there is a file to compare against, and
/// the whole planned file only under `--dry-run`.
fn show_change(
    out: &mut impl Write,
    plan: &HookInstall,
    changes: &[DiffLine],
    exists: bool,
) -> io::Result<()> {
    writeln!(out, "  Events it registers")?;
    for line in event_table(plan) {
        writeln!(out, "    {line}")?;
    }
    writeln!(out)?;
    writeln!(out, "  What changes in that file")?;
    if exists {
        render_diff(out, changes, 2, DIFF_LINE_BUDGET)?;
    } else {
        let added = changes
            .iter()
            .filter(|l| matches!(l, DiffLine::Added(_)))
            .count();
        writeln!(
            out,
            "    the file does not exist, so all {added} lines are new and nothing\n    \
             of yours can be overwritten"
        )?;
    }
    writeln!(out)?;
    Ok(())
}

/// `polis connect --uninstall`.
fn disconnect(out: &mut impl Write, settings: &Path, yes: bool) -> anyhow::Result<()> {
    writeln!(out)?;
    writeln!(out, "  Disconnect Polis from Claude Code")?;
    writeln!(out, "  ---------------------------------")?;
    writeln!(out)?;
    let plan = plan_uninstall(settings);
    writeln!(out, "    {}", settings.display())?;
    writeln!(out)?;
    for note in &plan.notes {
        writeln!(out, "  {note}")?;
    }
    if plan.removed.is_empty() {
        writeln!(out, "  Nothing of Polis's is registered there.")?;
        writeln!(out)?;
        out.flush()?;
        return Ok(());
    }
    writeln!(out, "  Removing {} Polis registrations", plan.removed.len())?;
    for line in wrap(&plan.removed.join(", "), 64) {
        writeln!(out, "    {line}")?;
    }
    if !plan.kept.is_empty() {
        writeln!(
            out,
            "  Leaving {} registration(s) Polis did not write",
            plan.kept.len()
        )?;
        for line in wrap(&plan.kept.join(", "), 64) {
            writeln!(out, "    {line}")?;
        }
    }
    writeln!(out)?;
    if plan.deletes_file {
        // No diff: "everything, and then the file" is not something a line-by-
        // line rendering makes clearer.
        writeln!(
            out,
            "  Nothing else is in that file and Polis created it, so the file\n  \
             itself is removed — which is exactly the state before `connect`."
        )?;
    } else {
        let before = std::fs::read_to_string(settings).unwrap_or_default();
        let after = plan.settings_json.clone().unwrap_or_default();
        render_diff(out, &diff(&before, &after), 2, DIFF_LINE_BUDGET)?;
    }
    writeln!(out)?;
    out.flush()?;

    if !confirm("  Remove them?", yes)? {
        let mut out = io::stdout().lock();
        writeln!(out, "  Nothing was changed.")?;
        out.flush()?;
        return Ok(());
    }
    let done = uninstall(settings);
    let mut out = io::stdout().lock();
    for note in &done.notes {
        writeln!(out, "  {note}")?;
    }
    if done.written {
        writeln!(out, "  Disconnected.")?;
    } else {
        writeln!(out, "  Nothing was changed.")?;
    }
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

/// What removing Polis's registrations from a settings file would do.
#[derive(Debug, Clone, Default)]
pub struct Uninstall {
    /// Event names whose Polis handler would be, or was, removed.
    pub removed: Vec<String>,
    /// Event names left alone because someone else wrote them.
    pub kept: Vec<String>,
    /// The file content afterwards, or `None` when the file is deleted.
    pub settings_json: Option<String>,
    /// Whether the file itself is removed — true only when Polis created it and
    /// nothing else was ever added to it.
    pub deletes_file: bool,
    /// Whether [`uninstall`] actually changed the filesystem.
    pub written: bool,
    /// Anything the operator should read.
    pub notes: Vec<String>,
}

/// Plans the removal without touching the filesystem.
///
/// Key by key and handler by handler, exactly as the install merges: another
/// tool's registration under an event Polis also uses must survive, and so must
/// every non-`hooks` key in the file.
pub fn plan_uninstall(settings: &Path) -> Uninstall {
    let mut plan = Uninstall::default();
    let Some(parsed) = read_json(settings) else {
        plan.notes
            .push(format!("{} does not exist.", settings.display()));
        return plan;
    };
    let Ok(Value::Object(mut settings_map)) = parsed else {
        plan.notes.push(format!(
            "{} is not a JSON object; refusing to touch it.",
            settings.display()
        ));
        if let Some(hint) = diagnose_unparseable(settings) {
            plan.notes.push(hint);
        }
        return plan;
    };

    let Some(Value::Object(hooks)) = settings_map.remove("hooks") else {
        plan.notes.push("no hooks block in that file.".to_owned());
        return plan;
    };
    let mut kept_hooks = Map::new();
    for (event, groups) in hooks {
        let Value::Array(groups) = groups else {
            plan.kept.push(event.clone());
            kept_hooks.insert(event, groups);
            continue;
        };
        let survivors: Vec<Value> = groups
            .into_iter()
            .filter(|group| !handler_is_polis(group))
            .collect();
        if survivors.is_empty() {
            plan.removed.push(event);
        } else {
            plan.kept.push(event.clone());
            kept_hooks.insert(event, Value::Array(survivors));
        }
    }
    if !kept_hooks.is_empty() {
        settings_map.insert("hooks".to_owned(), Value::Object(kept_hooks));
    }

    // A file that is empty afterwards and has no backup beside it is a file
    // Polis created: `install_hooks` only makes a backup when a file was already
    // there. Deleting it is what "put it back the way it was" means.
    plan.deletes_file = settings_map.is_empty() && !hookenv::backup_path(settings).exists();
    plan.settings_json = if plan.deletes_file {
        None
    } else {
        Some(format!(
            "{}\n",
            serde_json::to_string_pretty(&Value::Object(settings_map))
                .unwrap_or_else(|_| "{}".to_owned())
        ))
    };
    plan
}

/// Removes Polis's registrations. See [`plan_uninstall`] for the rules.
pub fn uninstall(settings: &Path) -> Uninstall {
    let mut plan = plan_uninstall(settings);
    if plan.removed.is_empty() {
        return plan;
    }
    match &plan.settings_json {
        None => match std::fs::remove_file(settings) {
            Ok(()) => plan.written = true,
            Err(error) => plan
                .notes
                .push(format!("cannot remove {}: {error}", settings.display())),
        },
        Some(json) => match std::fs::write(settings, json) {
            Ok(()) => plan.written = true,
            Err(error) => plan
                .notes
                .push(format!("cannot write {}: {error}", settings.display())),
        },
    }
    if plan.written {
        let backup = hookenv::backup_path(settings);
        if backup.exists() {
            plan.notes.push(format!(
                "the install's backup is still at {} if you want the original file back verbatim.",
                backup.display()
            ));
        }
    }
    plan
}

/// The three bytes a Windows editor puts at the front of a "UTF-8" file.
const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Why a settings file that looks fine in an editor will not parse.
///
/// One case, and it is not exotic: on Windows, `Out-File -Encoding utf8`,
/// Notepad's "UTF-8" and several editors' defaults all write a byte-order mark,
/// and strict JSON parsers reject the file at column 0 with a message about
/// EOF. Naming the cause is the difference between a two-second fix and an
/// afternoon.
fn diagnose_unparseable(settings: &Path) -> Option<String> {
    let bytes = std::fs::read(settings).ok()?;
    bytes.starts_with(UTF8_BOM).then(|| {
        format!(
            "That file starts with a UTF-8 byte-order mark (EF BB BF), which strict \
             JSON parsers reject — including this one. Re-save {} as UTF-8 with no \
             BOM and run polis connect again.",
            settings.display()
        )
    })
}

/// The two rules from `docs/verified/hooks-schema.md` §9.1 and §9.3, re-checked
/// against the JSON that is about to be written.
///
/// `polis-ingest` derives the block from `EventKind::ALL` and cannot produce a
/// `WorktreeCreate` entry, and this checks anyway. The failure mode being
/// guarded is not subtle: a `WorktreeCreate` handler replaces git's worktree
/// creation and a non-zero exit fails it, so every `git worktree`, every
/// `claude --worktree` and every isolated subagent on the machine stops working.
pub fn audit(settings_json: &str) -> anyhow::Result<()> {
    let value: Value =
        serde_json::from_str(settings_json).context("the planned settings file is not JSON")?;
    let Some(hooks) = value.get("hooks").and_then(Value::as_object) else {
        anyhow::bail!("the planned settings file has no hooks block");
    };
    anyhow::ensure!(
        !hooks.contains_key("WorktreeCreate"),
        "the planned hooks block registers WorktreeCreate, which replaces git's worktree \
         creation and fails it (hooks-schema.md §9.1). Refusing to write it."
    );
    let pre = hooks
        .get("PreToolUse")
        .and_then(Value::as_array)
        .context("the planned hooks block does not register PreToolUse (hooks-schema.md §9.3)")?;
    let matcher = pre
        .first()
        .and_then(|group| group.get("matcher"))
        .and_then(Value::as_str);
    anyhow::ensure!(
        matcher == Some(hookenv::PRE_TOOL_USE_MATCHER),
        "PreToolUse must be narrowed to {} — unmatched it is a ~200 call/sec firehose \
         (hooks-schema.md §9.3). Found {matcher:?}.",
        hookenv::PRE_TOOL_USE_MATCHER
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// polis doctor — every problem with its fix
// ---------------------------------------------------------------------------

/// How bad one [`Check`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Working.
    Ok,
    /// Working, but something will be missing or is worth knowing.
    Warn,
    /// Not working. Something the operator wants will not happen.
    Fail,
}

impl Status {
    /// The four-character tag printed at the start of the line.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Ok => "ok  ",
            Self::Warn => "warn",
            Self::Fail => "FAIL",
        }
    }
}

/// Something Polis can do about a [`Check`] without the operator typing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remedy {
    /// `cargo build --profile hook -p polis-hook`.
    BuildHook,
    /// `polis connect`.
    InstallHooks,
}

/// One line of `polis doctor`.
#[derive(Debug, Clone)]
pub struct Check {
    /// What was checked.
    pub name: &'static str,
    /// How it went.
    pub status: Status,
    /// What was found, in words.
    pub detail: String,
    /// The exact command that fixes it, when it is not already fine.
    pub fix: Option<String>,
    /// Whether `--fix` can apply it here.
    pub remedy: Option<Remedy>,
}

impl Check {
    /// A passing check.
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Ok,
            detail: detail.into(),
            fix: None,
            remedy: None,
        }
    }

    /// A check that found something, with the command that fixes it.
    fn problem(
        name: &'static str,
        status: Status,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            name,
            status,
            detail: detail.into(),
            fix: Some(fix.into()),
            remedy: None,
        }
    }

    /// This check, with `--fix` able to apply it.
    fn fixable(mut self, remedy: Remedy) -> Self {
        self.remedy = Some(remedy);
        self
    }
}

/// Runs every check. Pure inspection; nothing here writes.
pub fn checks(d: &Detected) -> Vec<Check> {
    let mut out = vec![
        check_on_path(),
        check_repository(d),
        check_claude(d),
        check_transcripts(d),
    ];
    out.extend(check_ports());
    out.push(check_hook_binary(d));
    out.push(check_hooks(d));
    out.push(check_telemetry_env());
    out.push(check_something_to_watch(d));
    out
}

/// Can this binary be typed as `polis`?
///
/// The first-run review's first blocker: the first command in the getting-started
/// guide is `polis`, and a fresh checkout builds to `target/release/polis` with
/// nothing on `PATH`. The guide's remedy was one clause ninety lines further
/// down — *"put that folder on your `PATH`"* — **with no command for doing so on
/// any platform**. This is that command, for this machine, with this binary's
/// own directory already substituted in.
///
/// A `Warn` rather than a `Fail`: everything works from a full path, and the
/// operator plainly ran *something*.
fn check_on_path() -> Check {
    let Ok(exe) = std::env::current_exe() else {
        return Check::ok("polis", "running from an unknown location");
    };
    let exe = crate::cli::strip_verbatim(exe.canonicalize().unwrap_or(exe));
    let dir = exe.parent().unwrap_or(&exe).to_path_buf();
    let same = |a: &Path| {
        a.canonicalize()
            .map(crate::cli::strip_verbatim)
            .is_ok_and(|found| found == exe)
    };
    match which("polis") {
        Some(found) if same(&found) => Check::ok("polis", format!("{} — on PATH", exe.display())),
        Some(found) => Check::problem(
            "polis",
            Status::Warn,
            format!(
                "typing `polis` runs {}, not this one ({})",
                found.display(),
                exe.display()
            ),
            format!(
                "put this one first, or run it by its full path:\n  {}",
                exe.display()
            ),
        ),
        None => Check::problem(
            "polis",
            Status::Warn,
            format!("not on PATH — this binary is {}", exe.display()),
            path_fix(&dir),
        ),
    }
}

/// The exact command that puts `dir` on `PATH`, for this platform.
///
/// Windows gets the user-scoped `SetEnvironmentVariable` form rather than
/// `setx PATH "%PATH%;…"`, which is the line most guides print and which is a
/// footgun: `%PATH%` there is the *combined* machine and user value, so it
/// copies the whole system path into the user one, permanently, truncated at
/// 1024 characters.
pub fn path_fix(dir: &Path) -> String {
    let dir = dir.display();
    if cfg!(windows) {
        format!(
            "add it to PATH for good (PowerShell; new terminals see it):\n  \
             [Environment]::SetEnvironmentVariable('Path', \
             [Environment]::GetEnvironmentVariable('Path','User') + ';{dir}', 'User')\n\
             or just for this terminal:\n  \
             $env:PATH = \"{dir};$env:PATH\""
        )
    } else {
        format!(
            "add it to PATH for this shell:\n  \
             export PATH=\"{dir}:$PATH\"\n\
             and to ~/.bashrc or ~/.zshrc to make it permanent."
        )
    }
}

/// Is there a city to draw? Without git history there is no growth order and so
/// no layout at all (PRD §7.1).
fn check_repository(d: &Detected) -> Check {
    if d.is_git {
        Check::ok(
            "repository",
            format!("{} — a git checkout", d.repo.display()),
        )
    } else {
        Check::problem(
            "repository",
            Status::Fail,
            format!("{} is not a git repository", d.repo.display()),
            "cd into a git checkout, or run:  polis --repo <path-to-a-repo>",
        )
    }
}

/// Is there an agent to watch?
fn check_claude(d: &Detected) -> Check {
    match &d.claude {
        Some(path) => Check::ok("Claude Code", path.display().to_string()),
        None => Check::problem(
            "Claude Code",
            Status::Warn,
            "no `claude` on PATH",
            "install it from https://claude.com/claude-code — Polis can still \
             replay sessions without it",
        ),
    }
}

/// Is there a corpus of past sessions?
fn check_transcripts(d: &Detected) -> Check {
    match (&d.projects_dir, d.projects_exists) {
        (Some(dir), true) => Check::ok(
            "transcripts",
            format!(
                "{} — {} sessions, {} replayable",
                dir.display(),
                d.sessions,
                d.replayable
            ),
        ),
        (Some(dir), false) => Check::problem(
            "transcripts",
            Status::Warn,
            format!("{} does not exist", dir.display()),
            "run Claude Code once in any repository; it creates that directory",
        ),
        (None, _) => Check::problem(
            "transcripts",
            Status::Warn,
            "no home directory to find ~/.claude/projects in",
            "set USERPROFILE (Windows) or HOME (elsewhere)",
        ),
    }
}

/// Can the two receivers bind?
///
/// A held 4317 is a `Fail` and a held hook port is a `Warn` for the same reason:
/// agents export to 4317 unconditionally, so something else holding it silently
/// eats the whole telemetry channel, while a second Polis on the hook port
/// splits one stream two ways and both halves still look healthy (ADR-0026).
fn check_ports() -> [Check; 2] {
    let otlp = polis_ingest::default_otlp_addr();
    let telemetry = match tcp_port_state(otlp) {
        Ok(()) => Check::ok("port 4317", format!("{otlp} is free for the receiver")),
        Err(error) => Check::problem(
            "port 4317",
            Status::Fail,
            format!("{otlp} is taken ({error})"),
            "something else is listening there — another Polis, or an OpenTelemetry \
             collector. Stop it, or agents will export into it instead",
        ),
    };
    let hook_addr = SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, DEFAULT_HOOK_PORT);
    let hooks = match udp_port_state(hook_addr) {
        Ok(()) => Check::ok("hook port", format!("{hook_addr} is free")),
        Err(error) => Check::problem(
            "hook port",
            Status::Warn,
            format!("{hook_addr} is taken ({error})"),
            "another Polis is already receiving hook events; stop it first, or the \
             two will each see about half",
        ),
    };
    [telemetry, hooks]
}

/// Has the hook transport been built?
fn check_hook_binary(d: &Detected) -> Check {
    match &d.hook_binary {
        Some(path) => Check::ok("polis-hook", path.display().to_string()),
        None => Check::problem(
            "polis-hook",
            Status::Warn,
            "not built — hooks cannot fire without it",
            "cargo build --profile hook -p polis-hook",
        )
        .fixable(Remedy::BuildHook),
    }
}

/// Are the hooks registered, and is there a dangerous one?
fn check_hooks(d: &Detected) -> Check {
    if d.worktree_hook {
        Check::problem(
            "hooks",
            Status::Fail,
            "a settings file registers WorktreeCreate, which replaces git's worktree \
             creation and fails it",
            "remove the WorktreeCreate entry by hand — Polis never writes one, and \
             while it is there every `git worktree` on this machine is broken",
        )
    } else if d.hooks_installed() {
        Check::ok(
            "hooks",
            format!(
                "{} registrations in this repo, {} for every repo",
                d.hooks_project, d.hooks_user
            ),
        )
    } else {
        Check::problem(
            "hooks",
            Status::Warn,
            "not installed — Polis will not see agents you start yourself",
            "polis connect",
        )
        .fixable(Remedy::InstallHooks)
    }
}

/// Is the agent environment exported here?
///
/// A `warn` that says "this is normal", because it usually is: the variables
/// only have to be set in the shell that starts `claude`, and `polis run` sets
/// them on the agent itself so that shell never needs them.
fn check_telemetry_env() -> Check {
    let wanted = hookenv::agent_env("http://127.0.0.1:4317");
    let total = wanted.len();
    let exported = wanted
        .into_iter()
        .filter(|(key, value)| std::env::var(key).as_deref() == Ok(value.as_str()))
        .count();
    if exported == total {
        Check::ok(
            "telemetry env",
            format!("all {total} variables set in this shell"),
        )
    } else {
        Check::problem(
            "telemetry env",
            Status::Warn,
            format!("{exported} of {total} variables set in this shell (this is normal)"),
            "you do not have to set them: `polis run -- claude` sets them on the \
             agent it starts. To do it by hand: polis env --shell powershell",
        )
    }
}

/// Is there anything to look at right now?
fn check_something_to_watch(d: &Detected) -> Check {
    if d.replayable > 0 {
        Check::ok(
            "something to watch",
            format!("{} sessions ready to replay — polis replay", d.replayable),
        )
    } else {
        Check::problem(
            "something to watch",
            Status::Warn,
            "no replayable sessions found",
            "start an agent in this repository (`claude` on its own is enough), then:              polis watch",
        )
    }
}

/// The "what can Polis see right now" half of `polis doctor` (PRD §15 M3).
///
/// First, and separate from the machine inventory below it, because an operator
/// runs this command for one of two reasons and they want different halves. "The
/// map is empty and I do not know why" is answered here, usually with *"nothing
/// is wrong; no agent is running in this repository"*. "I am setting Polis up"
/// is answered by the checks.
fn write_live_section(
    out: &mut impl Write,
    live: &crate::status::Connectivity,
) -> anyhow::Result<()> {
    writeln!(out)?;
    writeln!(out, "  what Polis can see right now")?;
    live.write(out)?;
    if let Some(roster) = &live.roster {
        writeln!(out)?;
        crate::watch::write_roster(out, roster)?;
    }
    Ok(())
}

/// One line per [`Check`], with its fix indented under it.
fn write_checks(out: &mut impl Write, list: &[Check]) -> anyhow::Result<()> {
    for check in list {
        writeln!(
            out,
            "  {}  {:<18}  {}",
            check.status.tag(),
            check.name,
            check.detail
        )?;
        if let Some(fix) = &check.fix {
            for (i, line) in fix_lines(fix).into_iter().enumerate() {
                let label = if i == 0 { "fix" } else { "" };
                writeln!(out, "        {label:<18}  {line}")?;
            }
        }
    }
    Ok(())
}

/// `polis doctor` (PRD §4).
pub fn doctor(cli: &Cli, args: &DoctorArgs) -> anyhow::Result<()> {
    let repo = cli.repo_root().context("resolving the repository root")?;
    let detected = detect(&repo);
    let list = checks(&detected);

    // What is and is not connected, before what is and is not installed: an
    // operator running `polis doctor` because the map looked empty needs the
    // answer to "why am I seeing less than I expected" first, and that answer is
    // usually "nothing is wrong, no agent is running here" (PRD §15 M3).
    let live = crate::status::Connectivity::inspect(&repo, polis_ingest::SessionScope::ThisRepo);

    let mut out = io::stdout().lock();
    writeln!(out)?;
    writeln!(out, "  polis doctor — {}", repo.display())?;
    write_live_section(&mut out, &live)?;
    writeln!(out)?;
    writeln!(out, "  this machine")?;
    write_checks(&mut out, &list)?;
    writeln!(out)?;

    let problems = list
        .iter()
        .filter(|c| c.status != Status::Ok)
        .collect::<Vec<_>>();
    if problems.is_empty() {
        writeln!(out, "  Everything checks out.")?;
        writeln!(out)?;
        out.flush()?;
        return Ok(());
    }
    // A warning and a failure must not read the same. The commonest healthy
    // machine has exactly one warning on it — the telemetry block is not
    // exported into the operator's shell, and it does not need to be — and
    // "1 problem" on a working install teaches people to ignore this command.
    let broken = problems.iter().filter(|c| c.status == Status::Fail).count();
    let fixable = problems.iter().filter(|c| c.remedy.is_some()).count();
    if broken == 0 {
        writeln!(
            out,
            "  Nothing is broken. {} above, {}.",
            plural(problems.len(), "note", "notes"),
            if problems.len() == 1 {
                "with what to do about it"
            } else {
                "each with what to do about it"
            }
        )?;
    } else {
        writeln!(
            out,
            "  {} that will stop something working, and {}.",
            plural(broken, "problem", "problems"),
            plural(problems.len() - broken, "note", "notes")
        )?;
    }
    if fixable > 0 {
        writeln!(
            out,
            "  polis doctor --fix can apply {} of them for you.",
            if fixable == 1 {
                "one".to_owned()
            } else {
                fixable.to_string()
            }
        )?;
    }
    writeln!(out)?;
    out.flush()?;

    if !args.fix {
        return Ok(());
    }
    for check in problems.iter().filter(|c| c.remedy.is_some()) {
        let mut out = io::stdout().lock();
        writeln!(out, "  fixing: {}", check.name)?;
        out.flush()?;
        drop(out);
        match check.remedy {
            Some(Remedy::BuildHook) => build_hook(&repo)?,
            Some(Remedy::InstallHooks) => {
                let connect_args = ConnectArgs {
                    user: false,
                    settings: None,
                    hook_binary: None,
                    dry_run: false,
                    yes: args.yes,
                    force: false,
                    uninstall: false,
                };
                connect(cli, &connect_args)?;
            }
            None => {}
        }
    }
    Ok(())
}

/// `cargo build --profile hook -p polis-hook`, run where the operator is.
fn build_hook(repo: &Path) -> anyhow::Result<()> {
    let cargo = which("cargo").context(
        "cargo is not on PATH, so polis-hook cannot be built here. Install Rust from \
         https://rustup.rs",
    )?;
    println!("  running: cargo build --profile hook -p polis-hook");
    let status = Command::new(cargo)
        .current_dir(repo)
        .args(["build", "--profile", "hook", "-p", "polis-hook"])
        .status()
        .context("running cargo")?;
    anyhow::ensure!(status.success(), "cargo build failed");
    Ok(())
}

/// Whether a TCP port can be bound right now.
fn tcp_port_state(addr: SocketAddr) -> Result<(), String> {
    std::net::TcpListener::bind(addr)
        .map(drop)
        .map_err(|e| e.to_string())
}

/// Whether a UDP port can be bound right now.
fn udp_port_state(addr: SocketAddrV4) -> Result<(), String> {
    UdpSocket::bind(addr).map(drop).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Finding and launching other programs
// ---------------------------------------------------------------------------

/// Resolves a program name against `PATH`, honouring `PATHEXT` on Windows.
///
/// `std::process::Command` cannot do this: on Windows it appends `.exe` and only
/// `.exe`, so a `claude` installed by npm — which is `claude.cmd` — is invisible
/// to it and `polis run -- claude` would fail with "program not found" on a
/// machine where `claude` works perfectly from the same shell.
pub fn which(program: &str) -> Option<PathBuf> {
    let raw = Path::new(program);
    if raw.is_absolute() || raw.components().count() > 1 {
        return raw.is_file().then(|| raw.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let direct = dir.join(program);
        if direct.is_file() && (!cfg!(windows) || raw.extension().is_some()) {
            return Some(direct);
        }
        for ext in path_extensions() {
            let candidate = dir.join(format!("{program}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// The extensions [`which`] appends. Empty on anything but Windows.
fn path_extensions() -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
        .split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Builds the [`Command`] that runs `program` with `args`.
///
/// A `.cmd` or `.bat` is a script, not an image, so `CreateProcess` cannot run
/// it: it goes through `cmd.exe /d /s /c "…"`, which is the one documented form
/// that treats the rest of the line verbatim. Everything else is spawned
/// directly.
pub fn command_for(program: &Path, args: &[std::ffi::OsString]) -> Command {
    #[cfg(windows)]
    {
        let script = program
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
        if script {
            use std::os::windows::process::CommandExt as _;
            let comspec =
                std::env::var_os("ComSpec").unwrap_or_else(|| std::ffi::OsString::from("cmd.exe"));
            let mut line = String::from("\"");
            line.push_str(&quote_for_cmd(&program.display().to_string()));
            for arg in args {
                line.push(' ');
                line.push_str(&quote_for_cmd(&arg.to_string_lossy()));
            }
            line.push('"');
            let mut command = Command::new(comspec);
            command.raw_arg("/d").raw_arg("/s").raw_arg("/c");
            command.raw_arg(line);
            return command;
        }
    }
    let mut command = Command::new(program);
    command.args(args);
    command
}

/// Quotes one token for a `cmd.exe /s /c` command line.
///
/// Kept next to [`command_for`] and compiled everywhere so it stays tested on
/// every platform, not only the one that uses it.
fn quote_for_cmd(token: &str) -> String {
    if !token.is_empty()
        && !token.contains([' ', '\t', '"', '&', '|', '<', '>', '^', '(', ')', ','])
    {
        return token.to_owned();
    }
    format!("\"{}\"", token.replace('"', "\"\""))
}

/// `<program> --version`, with a timeout so a slow agent cannot hang `doctor`.
pub fn probe_version(program: &Path) -> Option<String> {
    let program = program.to_path_buf();
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::Builder::new()
        .name("polis-version-probe".to_owned())
        .spawn(move || {
            let out = command_for(&program, &[std::ffi::OsString::from("--version")]).output();
            let _ = tx.send(out.ok());
        })
        .ok()?;
    let output = rx.recv_timeout(VERSION_TIMEOUT).ok()??;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().next().map(|l| l.trim().to_owned())
}

// ---------------------------------------------------------------------------
// Terminal manners
// ---------------------------------------------------------------------------

/// Asks a yes/no question.
///
/// With no terminal to ask, this refuses rather than assuming: `polis connect`
/// writes to a file the operator did not name, and "the pipe said nothing so I
/// wrote it" is not consent.
pub fn confirm(prompt: &str, assume_yes: bool) -> anyhow::Result<bool> {
    if assume_yes {
        println!("{prompt} [y/N] y   (--yes)");
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        anyhow::bail!(
            "this needs a yes or no and there is no terminal to ask on; re-run with --yes if \
             you mean it"
        );
    }
    print!("{prompt} [y/N] ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Whether a failure should wait for a keypress before the window disappears.
///
/// The bug this exists for: double-clicking `polis.exe` on Windows opens a
/// console, runs with no arguments, prints something and closes instantly, which
/// reads as a crash. There is no way to ask Windows "do I own this console"
/// without `unsafe`, which this workspace denies, so the rule is the one that
/// costs a keypress in the only case it is wrong: **no arguments, a terminal on
/// stdin, and something went wrong.** A double-clicked binary matches. A shell
/// with a script or a pipe does not. `POLIS_NO_PAUSE=1` turns it off outright,
/// and the launcher scripts set it because they do their own pause.
pub fn should_pause(had_arguments: bool) -> bool {
    !had_arguments
        && std::env::var_os("POLIS_NO_PAUSE").is_none()
        // A window Polis spawned for itself has nobody at it. Its stdin is
        // already `null`, so the terminal test below would catch it too; this is
        // here so the rule survives the day that changes.
        && std::env::var_os(crate::run::WINDOW_CHILD_ENV).is_none()
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
}

/// Waits for a line, so a double-clicked window can be read before it closes.
pub fn pause() {
    println!();
    print!("  Press Enter to close this window. ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
}

// ---------------------------------------------------------------------------
// A small line diff, so `connect` can show its work
// ---------------------------------------------------------------------------

/// One line of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    /// Unchanged.
    Same(String),
    /// Only in the old text.
    Removed(String),
    /// Only in the new text.
    Added(String),
}

/// The largest input [`diff`] runs the quadratic algorithm on.
///
/// A settings file is tens of lines; this only exists so that pointing `connect`
/// at something enormous degrades into a summary instead of allocating a
/// gigabyte.
const DIFF_CAP: usize = 1_200;

/// A longest-common-subsequence line diff.
pub fn diff(before: &str, after: &str) -> Vec<DiffLine> {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    if old.len() > DIFF_CAP || new.len() > DIFF_CAP {
        let mut out: Vec<DiffLine> = old
            .iter()
            .map(|l| DiffLine::Removed((*l).to_owned()))
            .collect();
        out.extend(new.iter().map(|l| DiffLine::Added((*l).to_owned())));
        return out;
    }
    let width = new.len() + 1;
    let mut lcs = vec![0u32; (old.len() + 1) * width];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            lcs[i * width + j] = if old[i] == new[j] {
                lcs[(i + 1) * width + j + 1] + 1
            } else {
                lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < old.len() && j < new.len() {
        if old[i] == new[j] {
            out.push(DiffLine::Same(old[i].to_owned()));
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * width + j] >= lcs[i * width + j + 1] {
            out.push(DiffLine::Removed(old[i].to_owned()));
            i += 1;
        } else {
            out.push(DiffLine::Added(new[j].to_owned()));
            j += 1;
        }
    }
    out.extend(old[i..].iter().map(|l| DiffLine::Removed((*l).to_owned())));
    out.extend(new[j..].iter().map(|l| DiffLine::Added((*l).to_owned())));
    out
}

/// Prints a diff with `context` unchanged lines around each change, stopping
/// after `budget` printed lines and saying how many it did not print.
///
/// The budget is what makes this a consent screen rather than a wall: nobody
/// reads two hundred lines of generated JSON before typing `y`, and a screen
/// that scrolls the question off the top is worse than no screen.
pub fn render_diff(
    out: &mut impl Write,
    lines: &[DiffLine],
    context: usize,
    budget: usize,
) -> io::Result<()> {
    let mut show = vec![false; lines.len()];
    for i in 0..lines.len() {
        if matches!(lines[i], DiffLine::Same(_)) {
            continue;
        }
        let lo = i.saturating_sub(context);
        let hi = (i + context + 1).min(lines.len());
        for slot in &mut show[lo..hi] {
            *slot = true;
        }
    }
    let (mut added, mut removed) = (0usize, 0usize);
    let mut skipped = 0usize;
    let mut printed = 0usize;
    let mut elided = 0usize;
    for (i, line) in lines.iter().enumerate() {
        match line {
            DiffLine::Added(_) => added += 1,
            DiffLine::Removed(_) => removed += 1,
            DiffLine::Same(_) => {}
        }
        if !show[i] {
            skipped += 1;
            continue;
        }
        if printed >= budget {
            elided += 1;
            continue;
        }
        if skipped > 0 {
            writeln!(out, "      … {skipped} unchanged line(s)")?;
            skipped = 0;
            printed += 1;
        }
        match line {
            DiffLine::Same(text) => writeln!(out, "      {}", truncate(text, 68))?,
            DiffLine::Removed(text) => writeln!(out, "    - {}", truncate(text, 68))?,
            DiffLine::Added(text) => writeln!(out, "    + {}", truncate(text, 68))?,
        }
        printed += 1;
    }
    if skipped > 0 {
        writeln!(out, "      … {skipped} unchanged line(s)")?;
    }
    if elided > 0 {
        writeln!(
            out,
            "      … {elided} more changed line(s); --dry-run prints the whole file"
        )?;
    }
    writeln!(out, "    {added} line(s) added, {removed} removed")?;
    Ok(())
}

/// Clips a line so the diff cannot wrap and become unreadable.
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let head: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{head}…")
}

/// `"1 note"` / `"3 notes"`. Nothing in `polis doctor` says "thing(s)".
fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("{count} {one}")
    } else {
        format!("{count} {many}")
    }
}

/// Wraps prose to `width` on word boundaries, for the `fix:` column.
/// A fix as printed lines: prose is wrapped, a command is not.
///
/// A fix is allowed explicit newlines, and a line that starts with two spaces is
/// a command to copy. Those are emitted verbatim however long they are, because
/// [`wrap`] breaking a `PATH` one-liner at a space produces something that looks
/// like a command and does not run.
fn fix_lines(fix: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in fix.split('\n') {
        if line.starts_with("  ") {
            out.push(line.trim_end().to_owned());
        } else {
            out.extend(wrap(line, 62));
        }
    }
    out
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// The registrations as three or four short lines, most-important first.
///
/// This is what the operator actually has to consent to. The generated JSON is
/// 291 lines and says the same thing nineteen times; the two entries that carry
/// a matcher are the only ones with a decision in them, so they are named
/// separately and the rest are a list.
pub fn event_table(plan: &HookInstall) -> Vec<String> {
    let table = registrations(plan);
    let mut rendered = Vec::new();
    let mut unfiltered = Vec::new();
    for (event, matcher) in &table {
        match matcher {
            Some(matcher) => rendered.push(format!("{event}  only {matcher}")),
            None => unfiltered.push(event.clone()),
        }
    }
    rendered.push(format!("{} more, unfiltered:", unfiltered.len()));
    for chunk in unfiltered.chunks(4) {
        rendered.push(format!("  {}", chunk.join(", ")));
    }
    rendered
}

/// How many diff lines `polis connect` prints before it summarises the rest.
///
/// A consent screen that scrolls is a consent screen nobody read.
const DIFF_LINE_BUDGET: usize = 24;

/// The plan's registrations as a `event -> matcher` table, for tests and for
/// anything that wants to assert the shape of what was written.
pub fn registrations(plan: &HookInstall) -> BTreeMap<String, Option<String>> {
    let mut table = BTreeMap::new();
    let Ok(Value::Object(value)) = serde_json::from_str::<Value>(&plan.settings_json) else {
        return table;
    };
    let Some(Value::Object(hooks)) = value.get("hooks") else {
        return table;
    };
    for (event, groups) in hooks {
        let matcher = groups
            .as_array()
            .and_then(|g| g.first())
            .and_then(|g| g.get("matcher"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        table.insert(event.clone(), matcher);
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook_path() -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            "C:\\polis\\polis-hook.exe"
        } else {
            "/usr/local/bin/polis-hook"
        })
    }

    /// `hooks-schema.md` §9.1: a `WorktreeCreate` handler replaces git's
    /// worktree creation and then fails it. This is the check that stands
    /// between that and the operator's machine.
    #[test]
    fn the_written_block_never_registers_worktree_create() {
        let dir = crate::testutil::scratch("connect-audit");
        let settings = dir.join("settings.json");
        let plan = hookenv::plan_hooks(&settings, &hook_path());
        audit(&plan.settings_json).expect("the real plan must pass its own audit");

        let table = registrations(&plan);
        assert!(
            !table.contains_key("WorktreeCreate"),
            "WorktreeCreate must never be registered: {table:?}"
        );
        assert_eq!(table.len(), 19, "19 registrations: {table:?}");
        assert_eq!(
            table.get("PreToolUse").and_then(Clone::clone).as_deref(),
            Some(hookenv::PRE_TOOL_USE_MATCHER),
            "PreToolUse must be narrowed, and anchored so `Edit` does not also \
             match `NotebookEdit`"
        );
    }

    /// The audit has to actually reject the thing it exists to reject,
    /// otherwise it is decoration.
    #[test]
    fn the_audit_rejects_a_worktree_hook_and_a_wide_pretooluse() {
        let bad = r#"{"hooks":{"WorktreeCreate":[{"hooks":[]}],
            "PreToolUse":[{"matcher":"^(Edit|Write|NotebookEdit)$","hooks":[]}]}}"#;
        let error = audit(bad).expect_err("a WorktreeCreate hook must be refused");
        assert!(error.to_string().contains("WorktreeCreate"), "{error}");

        let wide = r#"{"hooks":{"PreToolUse":[{"matcher":"Edit","hooks":[]}]}}"#;
        let error = audit(wide).expect_err("an unanchored matcher must be refused");
        assert!(error.to_string().contains("PreToolUse"), "{error}");

        let none = r#"{"hooks":{"SessionStart":[{"hooks":[]}]}}"#;
        assert!(
            audit(none).is_err(),
            "a missing PreToolUse is a refusal too"
        );
    }

    /// The uninstall must put the file back. Compared as parsed JSON rather than
    /// bytes because the install rewrites the file pretty-printed, so byte
    /// equality would be a test of `serde_json`'s formatter.
    #[test]
    fn uninstall_restores_a_settings_file_that_already_existed() {
        let dir = crate::testutil::scratch("connect-roundtrip");
        let settings = dir.join("settings.json");
        let original = serde_json::json!({
            "permissions": { "allow": ["Bash(git status)"] },
            "model": "opus",
            "hooks": {
                // Somebody else's registration, under an event Polis never
                // touches. It has to be there afterwards, byte for byte.
                "SomeOtherEvent": [{ "hooks": [{ "type": "command",
                                                 "command": "other" }] }]
            }
        });
        std::fs::write(
            &settings,
            format!("{}\n", serde_json::to_string_pretty(&original).unwrap()),
        )
        .unwrap();

        let done = hookenv::install_hooks(&settings, &hook_path());
        assert!(done.written, "{:?}", done.notes);
        let (installed, worktree) = count_polis_hooks(&settings);
        assert_eq!(installed, 19, "every event registered");
        assert!(!worktree, "no WorktreeCreate, ever");

        let undo = uninstall(&settings);
        assert!(undo.written, "{:?}", undo.notes);
        assert!(!undo.deletes_file, "the file existed before, so it stays");
        let back: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(back, original, "uninstall must put the file back");
        assert_eq!(count_polis_hooks(&settings), (0, false));
    }

    /// The case the round-trip above deliberately excludes: somebody else's
    /// handler under an event Polis *also* registers cannot survive the install,
    /// because the merge is per event and replaces the whole array. So `connect`
    /// must refuse rather than write it — an uninstall cannot give back what an
    /// install destroyed, and `--force` plus the backup file is the only honest
    /// way through.
    #[test]
    fn a_handler_polis_did_not_write_is_a_refusal_not_a_silent_replacement() {
        let dir = crate::testutil::scratch("connect-conflict");
        let settings = dir.join("settings.json");
        std::fs::write(
            &settings,
            serde_json::json!({
                "hooks": {
                    "SessionStart": [{ "hooks": [{ "type": "command",
                                                   "command": "their-tool" }] }]
                }
            })
            .to_string(),
        )
        .unwrap();

        let plan = hookenv::plan_hooks(&settings, &hook_path());
        assert!(plan.has_conflicts(), "{plan:?}");
        assert!(
            plan.conflicts.contains(&"SessionStart".to_owned()),
            "{:?}",
            plan.conflicts
        );

        // And when it is forced through, the backup is what makes it reversible.
        let done = hookenv::install_hooks(&settings, &hook_path());
        assert!(done.written);
        let backup = done.backup.expect("a file that existed is backed up first");
        assert!(backup.exists(), "{}", backup.display());
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&backup).unwrap()).unwrap();
        assert_eq!(
            saved["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "their-tool"
        );
    }

    /// And when Polis created the file, "put it back" means there is no file.
    #[test]
    fn uninstall_removes_a_settings_file_polis_created() {
        let dir = crate::testutil::scratch("connect-created");
        let settings = dir.join("nested").join("settings.json");
        assert!(hookenv::install_hooks(&settings, &hook_path()).written);
        assert!(settings.exists());

        let undo = uninstall(&settings);
        assert!(undo.deletes_file && undo.written, "{undo:?}");
        assert!(!settings.exists(), "the file Polis created is gone");

        // And a second uninstall is a no-op, not an error.
        let again = uninstall(&settings);
        assert!(!again.written && again.removed.is_empty());
    }

    /// A settings file that looks fine in an editor and will not parse. Windows
    /// PowerShell's `Out-File -Encoding utf8` writes this, so it is not an edge
    /// case — the first hand-written settings file this feature was tested
    /// against had one.
    #[test]
    fn a_byte_order_mark_is_named_rather_than_reported_as_eof() {
        let dir = crate::testutil::scratch("connect-bom");
        let settings = dir.join("settings.json");
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(br#"{"model":"opus"}"#);
        std::fs::write(&settings, &bytes).unwrap();

        let hint = diagnose_unparseable(&settings).expect("a BOM must be diagnosed");
        assert!(hint.contains("byte-order mark"), "{hint}");
        assert!(hint.contains("EF BB BF"), "{hint}");

        // Reading is tolerant, so `doctor` still reports the file's hooks
        // correctly rather than calling it corrupt.
        assert!(matches!(read_json(&settings), Some(Ok(Value::Object(_)))));
        assert_eq!(count_polis_hooks(&settings), (0, false));

        // A file with no BOM that simply does not parse gets no BOM story.
        let broken = dir.join("broken.json");
        std::fs::write(&broken, "{ not json").unwrap();
        assert_eq!(diagnose_unparseable(&broken), None);
        assert!(matches!(read_json(&broken), Some(Err(_))));
    }

    #[test]
    fn the_diff_shows_only_what_changed() {
        let before = "a\nb\nc\n";
        let after = "a\nB\nc\n";
        let lines = diff(before, after);
        assert_eq!(
            lines,
            vec![
                DiffLine::Same("a".to_owned()),
                DiffLine::Removed("b".to_owned()),
                DiffLine::Added("B".to_owned()),
                DiffLine::Same("c".to_owned()),
            ]
        );

        // Identical inputs produce no change at all, which is what tells
        // `connect` it has nothing to do.
        assert!(diff(before, before)
            .iter()
            .all(|l| matches!(l, DiffLine::Same(_))));

        let mut rendered = Vec::new();
        render_diff(&mut rendered, &lines, 1, DIFF_LINE_BUDGET).unwrap();
        let text = String::from_utf8(rendered).unwrap();
        assert!(text.contains("- b"), "{text}");
        assert!(text.contains("+ B"), "{text}");
        assert!(text.contains("1 line(s) added, 1 removed"), "{text}");
    }

    #[test]
    fn a_huge_diff_degrades_instead_of_allocating_forever() {
        let big = "x\n".repeat(DIFF_CAP + 10);
        let lines = diff(&big, "y\n");
        assert!(lines.iter().any(|l| matches!(l, DiffLine::Added(_))));
        assert!(lines.iter().all(|l| !matches!(l, DiffLine::Same(_))));
    }

    #[test]
    fn checks_name_a_fix_for_every_problem_they_report() {
        let detected = Detected {
            repo: PathBuf::from("/nowhere"),
            is_git: false,
            projects_dir: None,
            projects_exists: false,
            sessions: 0,
            replayable: 0,
            repositories: 0,
            newest: None,
            sessions_here: 0,
            claude: None,
            hook_binary: None,
            hooks_project: 0,
            hooks_user: 0,
            worktree_hook: false,
        };
        let list = checks(&detected);
        assert!(list.len() >= 8, "{list:?}");
        for check in &list {
            if check.status == Status::Ok {
                continue;
            }
            let fix = check
                .fix
                .as_ref()
                .unwrap_or_else(|| panic!("{} reports a problem with no fix", check.name));
            assert!(!fix.trim().is_empty(), "{} has an empty fix", check.name);
        }
        // The two that `--fix` can act on are named as such.
        let fixable: Vec<_> = list.iter().filter_map(|c| c.remedy).collect();
        assert!(fixable.contains(&Remedy::BuildHook), "{fixable:?}");
        assert!(fixable.contains(&Remedy::InstallHooks), "{fixable:?}");
    }

    /// A `WorktreeCreate` entry someone else wrote is the most dangerous thing
    /// `doctor` can find, so it must outrank "hooks are installed".
    #[test]
    fn a_worktree_hook_is_reported_as_a_failure() {
        let detected = Detected {
            repo: PathBuf::from("/repo"),
            is_git: true,
            projects_dir: None,
            projects_exists: false,
            sessions: 0,
            replayable: 0,
            repositories: 0,
            newest: None,
            sessions_here: 0,
            claude: None,
            hook_binary: None,
            hooks_project: 19,
            hooks_user: 0,
            worktree_hook: true,
        };
        let hooks = checks(&detected)
            .into_iter()
            .find(|c| c.name == "hooks")
            .expect("a hooks check");
        assert_eq!(hooks.status, Status::Fail);
        assert!(hooks.detail.contains("WorktreeCreate"), "{hooks:?}");
    }

    /// The double-click rule. A run that was given arguments came from a shell
    /// or a script, and a shell keeps its own console — so it must never be made
    /// to wait, whatever else is true.
    #[test]
    fn a_run_with_arguments_never_waits_for_a_keypress() {
        assert!(
            !should_pause(true),
            "an invocation with arguments came from a shell, which does not vanish"
        );
        // Under `cargo test` stdin is not a terminal, so even the bare case is
        // false here: the pause can never fire in CI or a pipe.
        assert!(!should_pause(false));
    }

    #[test]
    fn nothing_in_doctor_says_thing_s() {
        assert_eq!(plural(1, "note", "notes"), "1 note");
        assert_eq!(plural(0, "note", "notes"), "0 notes");
        assert_eq!(plural(3, "problem", "problems"), "3 problems");
    }

    #[test]
    fn which_finds_a_program_by_its_full_path_and_rejects_a_missing_one() {
        let exe = std::env::current_exe().unwrap();
        assert_eq!(which(&exe.display().to_string()), Some(exe));
        assert_eq!(which("definitely-not-a-real-program-9f3a"), None);
    }

    #[test]
    fn a_cmd_shim_is_quoted_for_cmd_exe() {
        assert_eq!(quote_for_cmd("simple"), "simple");
        assert_eq!(
            quote_for_cmd(r"C:\Program Files\claude.cmd"),
            "\"C:\\Program Files\\claude.cmd\""
        );
        // An embedded quote is doubled, which is cmd's escape, not a backslash.
        assert_eq!(quote_for_cmd(r#"say "hi""#), r#""say ""hi""""#);
        // `&` would otherwise start a second command.
        assert_eq!(quote_for_cmd("a&b"), "\"a&b\"");
    }

    #[test]
    fn the_orientation_screen_explains_the_map_without_jargon() {
        let detected = Detected {
            repo: PathBuf::from("/repo"),
            is_git: true,
            projects_dir: Some(PathBuf::from("/home/me/.claude/projects")),
            projects_exists: true,
            sessions: 187,
            replayable: 184,
            repositories: 12,
            newest: None,
            sessions_here: 3,
            claude: Some(PathBuf::from("/usr/bin/claude")),
            hook_binary: None,
            hooks_project: 0,
            hooks_user: 0,
            worktree_hook: false,
        };
        let mut buffer = Vec::new();
        orientation(&mut buffer, &detected).unwrap();
        next_steps(&mut buffer, &detected).unwrap();
        let text = String::from_utf8(buffer).unwrap();
        for phrase in [
            "building is a file",
            "district is a directory",
            "uncommitted work",
            "187 past sessions",
            "polis watch",
            "polis run -- claude",
            "polis connect",
        ] {
            assert!(text.contains(phrase), "missing {phrase:?} in:\n{text}");
        }
        for jargon in ["OTLP", "territory", "KDE", "iso-contour", "PRD"] {
            assert!(!text.contains(jargon), "jargon {jargon:?} in:\n{text}");
        }
        // One screen: 24 lines is the smallest terminal anyone still uses.
        assert!(
            text.lines().count() <= 40,
            "the first screen is {} lines",
            text.lines().count()
        );
    }

    /// The blocker this exists for is that the guide said "put that folder on
    /// your PATH" and gave no command. Whatever else changes, the fix has to
    /// name the folder and be one line somebody can paste.
    #[test]
    fn the_path_fix_is_a_command_with_the_folder_already_in_it() {
        let dir = PathBuf::from(if cfg!(windows) {
            r"C:\coding\agentolis\target\release"
        } else {
            "/home/op/agentolis/target/release"
        });
        let fix = path_fix(&dir);
        assert!(
            fix.contains(&dir.display().to_string()),
            "the folder is not in it:\n{fix}"
        );
        let lines = fix_lines(&fix);
        let commands: Vec<&String> = lines.iter().filter(|l| l.starts_with("  ")).collect();
        assert!(commands.len() >= 2, "{lines:?}");
        // A command is printed whole, however long — a wrapped one does not run.
        for command in &commands {
            assert!(
                command.contains(&dir.display().to_string()),
                "a broken command: {command}"
            );
        }
        if cfg!(windows) {
            // `setx PATH "%PATH%;…"` copies the machine path into the user one
            // and truncates it at 1024 characters. Never that.
            assert!(!fix.contains("setx"), "{fix}");
            assert!(fix.contains("SetEnvironmentVariable"), "{fix}");
        } else {
            assert!(fix.contains("export PATH="), "{fix}");
        }
    }

    /// Prose wraps; a command does not.
    #[test]
    fn a_fix_wraps_its_prose_and_never_its_commands() {
        let long = "  some --very-long-command --with 'a lot of arguments' \
                    --and-a-path /home/somebody/with/a/deep/directory/tree/inside";
        let fix = format!("do this thing, which is explained in a sentence long enough to need wrapping at sixty-two columns:\n{long}");
        let lines = fix_lines(&fix);
        assert!(lines.len() >= 3, "the prose did not wrap: {lines:?}");
        assert!(
            lines.iter().any(|l| l.trim() == long.trim()),
            "the command was broken up: {lines:?}"
        );
    }

    /// `polis connect` writes the *project* file by default, and until this was
    /// on screen nothing anywhere said the choice existed.
    #[test]
    fn the_consent_screen_names_the_scope_and_the_other_one() {
        let mut buffer = Vec::new();
        explain_connect(
            &mut buffer,
            Path::new("/repo/.claude/settings.json"),
            Path::new("/bin/polis-hook"),
            false,
            Scope::Project,
        )
        .unwrap();
        let text = String::from_utf8(buffer).unwrap();
        // The prose is wrapped, so it is asserted at its source; the command is
        // on its own line and must appear whole.
        assert!(
            Scope::Project
                .describe()
                .0
                .contains("this repository's own"),
            "{text}"
        );
        assert!(text.contains("polis connect --user"), "{text}");

        let mut buffer = Vec::new();
        explain_connect(
            &mut buffer,
            Path::new("/home/me/.claude/settings.json"),
            Path::new("/bin/polis-hook"),
            true,
            Scope::User,
        )
        .unwrap();
        let text = String::from_utf8(buffer).unwrap();
        assert!(
            Scope::User
                .describe()
                .0
                .contains("every repository on this machine"),
            "{text}"
        );
        // And the way back to the project file, whole and copyable.
        assert!(text.lines().any(|l| l.trim() == "polis connect"), "{text}");
    }

    #[test]
    fn wrapping_never_loses_a_word() {
        let text = "cd into a git checkout, or run: polis --repo <path-to-a-repo>";
        let lines = wrap(text, 24);
        assert!(lines
            .iter()
            .all(|l| l.chars().count() <= 24 || !l.contains(' ')));
        assert_eq!(lines.join(" "), text);
    }
}
