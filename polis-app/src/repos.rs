//! Which repository this window is about, and how to change it (PRD §2, §15 M3).
//!
//! PRD §2 is emphatic that **one Polis window maps one repository**, and that is
//! unchanged: nothing here draws two cities at once. What it changes is the cost
//! of choosing which one, which used to be a command line and a restart.
//!
//! Two facts made that expensive. An operator runs agents in more than one
//! checkout — the roster already knew this, and `polis watch` printed
//! *"Elsewhere on this machine — reported, not drawn"* followed by the exact
//! `polis -C <that repository> watch` to type. And the repository the operator
//! wants is almost never the folder their shell happens to be standing in; it is
//! *the one with two agents working in it right now*, which the shell cannot
//! know and this can.
//!
//! So this module answers one question — **where could Polis be looking, and
//! what is happening there** — and answers it the same way for both places that
//! ask:
//!
//! * bare `polis`, which opens [`Launcher`] as the front door
//!   ([`crate::app::Mode::Home`]), and
//! * the repo button in an open window's title bar, which opens the *same*
//!   launcher over the map and switches the watch in place.
//!
//! # Where the list comes from
//!
//! Three sources, merged by checkout root:
//!
//! 1. **Live sessions** — [`polis_ingest::live::LiveTailer`] with
//!    [`Scope::Everything`], which is what `polis watch --list` already reads.
//!    This is the source that matters: it is the only one that knows an agent is
//!    working in `C:\work\my-api` right now.
//! 2. **Session history** — [`SessionIndex`] over `~/.claude/projects`, headers
//!    only, for repositories worked in before but quiet now.
//! 3. **Recents** — the checkouts Polis has opened on this machine, in
//!    `<state dir>/recent-repos.json`. The only one Polis writes.
//!
//! The first two report a session's `cwd`, which is not the checkout root: an
//! agent started in `repo/crates/thing` reports that. [`checkout_containing`]
//! walks up to the `.git` so the sources agree on one row per repository rather
//! than one per subdirectory an agent happened to start in.
//!
//! # It scans on a thread, and rescans on a clock
//!
//! The scan is the same work `polis watch --list` does and it binds nothing, so
//! it is safe beside a running watch — but it reads every project directory on
//! the machine, so it never runs inside a frame. While the launcher is open it
//! repeats every [`RESCAN`]: the whole point of the live column is that it is
//! live, and an operator watching for an agent to appear must not have to close
//! and reopen the screen to see it. The previous list stays on screen while a
//! rescan runs, so the list does not flash back to a spinner every two seconds.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use eframe::egui::{self, RichText};
use polis_events::WallTime;
use polis_ingest::live::{Activity, LiveTailer, Roster, Scope};
use polis_world::sessions::{IndexOptions, SessionIndex};

use crate::palette;
use crate::session::Nav;

/// How often the launcher rescans while it is on screen.
///
/// The live column is the reason this screen exists, so it has to move on its
/// own. Two seconds is well inside the reaction time of "start an agent in
/// another terminal, then look back at Polis", and the scan is the one
/// `polis watch --list` runs — hundreds of milliseconds of `read_dir`, on a
/// background thread, never on the frame.
pub const RESCAN: Duration = Duration::from_secs(2);

/// How often the window wakes while a scan is in flight.
pub const POLL: Duration = Duration::from_millis(80);

/// How many checkouts the recents file remembers.
///
/// Enough that a week of work fits, and not so many that the list stops being a
/// list. A repository that falls off is not lost: it is still in the session
/// history, which is the second source.
const RECENTS: usize = 24;

/// The file the recents live in, under [`crate::config::Config::state_dir`].
const RECENTS_FILE: &str = "recent-repos.json";

// ---------------------------------------------------------------------------
// One repository
// ---------------------------------------------------------------------------

/// A checkout Polis could open, and what is happening in it.
///
/// The four flags are four independent facts about one directory — it is there,
/// it is a checkout, it is the one being watched, it has been opened before —
/// and every row shows all four. An enum over them would have to invent an
/// order they do not have.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    /// The checkout root — the directory holding `.git`.
    pub root: PathBuf,
    /// Sessions working in it right now.
    pub working: usize,
    /// Sessions live but quiet — waiting for their operator, or closed. The
    /// transcript cannot tell those apart; a `Stop` hook can.
    pub idle: usize,
    /// Sessions this machine has ever recorded against it.
    pub sessions: usize,
    /// When one of them last wrote a line.
    pub last: Option<WallTime>,
    /// Whether the directory is still on disk.
    pub exists: bool,
    /// Whether it is a git checkout. Without one there is no growth order and
    /// therefore no city (PRD §7.1), so such a row is shown and refused rather
    /// than hidden.
    pub git: bool,
    /// Whether this is the repository the window is already about.
    pub here: bool,
    /// Whether Polis has opened it before.
    pub recent: bool,
}

impl Place {
    /// The directory name, which is what an operator calls a repository.
    pub fn name(&self) -> String {
        self.root.file_name().map_or_else(
            || self.root.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    }

    /// Sessions that are live in it, working or not.
    pub fn live(&self) -> usize {
        self.working + self.idle
    }

    /// The live column, in words.
    ///
    /// An em dash rather than `0 agents`: quiet is the normal state of most
    /// rows, and a column of zeroes reads as a broken count.
    pub fn agents(&self) -> String {
        match (self.working, self.idle) {
            (0, 0) => "—".to_owned(),
            (0, idle) => format!("{idle} idle"),
            (working, 0) => format!("{working} working"),
            (working, idle) => format!("{working} working, {idle} idle"),
        }
    }

    /// Whether Polis can open it at all.
    pub fn openable(&self) -> bool {
        self.exists && self.git
    }

    /// Why it cannot be opened, in the operator's words.
    ///
    /// Shown on the row rather than discovered after a click: a picker that
    /// offers a row and then fails on it is a picker that gets clicked twice.
    pub fn refusal(&self) -> Option<&'static str> {
        match (self.exists, self.git) {
            (false, _) => Some("the folder is gone"),
            (_, false) => Some("not a git checkout"),
            _ => None,
        }
    }
}

/// What one scan found.
#[derive(Debug, Clone, Default)]
pub struct Survey {
    /// Every checkout worth offering, best first.
    pub places: Vec<Place>,
    /// Where sessions were looked for, for the empty state.
    pub projects_dir: Option<PathBuf>,
    /// Why the scan saw less than it should have. Never fatal: a machine with no
    /// `~/.claude/projects` has simply not run an agent yet.
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Finding the checkout
// ---------------------------------------------------------------------------

/// The checkout `dir` is inside, if any.
///
/// Walks up looking for `.git`, which is a **directory** in a normal clone and a
/// **file** in a worktree or a submodule — so the test is `exists`, not
/// `is_dir`, and PRD §7.6's worktrees resolve to themselves rather than to the
/// primary checkout they point at.
///
/// No `git rev-parse`: every session's `cwd` would want one, this runs over
/// every session on the machine every [`RESCAN`], and a process spawn per row
/// per two seconds is not a price a picker should charge.
pub fn checkout_containing(dir: &Path) -> Option<PathBuf> {
    let mut at = Some(dir);
    while let Some(path) = at {
        if path.join(".git").exists() {
            return Some(path.to_path_buf());
        }
        at = path.parent();
    }
    None
}

/// The key two paths are the same repository under.
///
/// Windows paths are case-insensitive and separator-agnostic while transcripts
/// carry whatever the operator typed, so `C:\coding\agentolis` and
/// `c:/coding/Agentolis` are one repository and must not become two rows.
/// Elsewhere case and slashes are both meaningful and are left alone.
fn key(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    } else {
        text.trim_end_matches('/').to_owned()
    }
}

// ---------------------------------------------------------------------------
// The scan
// ---------------------------------------------------------------------------

/// Reads all three sources and merges them. Binds nothing; safe beside a watch.
pub fn scan(here: Option<&Path>, state_dir: Option<&Path>) -> Survey {
    let projects_dir = polis_ingest::default_claude_projects_dir();
    let recents = recent(state_dir);
    let (roster, index, mut error) = match projects_dir.as_deref() {
        None => (
            None,
            None,
            Some("no home directory to find ~/.claude/projects in".to_owned()),
        ),
        Some(dir) if !dir.is_dir() => (
            None,
            None,
            Some(format!(
                "{} does not exist yet — run Claude Code once in any repository and it \
                 appears here",
                dir.display()
            )),
        ),
        Some(dir) => {
            // The same one-shot read `polis watch --list` and `polis doctor`
            // take, for the same reason: it holds no port, so it is safe beside
            // a watch that holds both of them.
            let roster = LiveTailer::open(dir, Scope::Everything).roster();
            let index = SessionIndex::scan_with(dir, &IndexOptions::quick()).ok();
            (Some(roster), index, None)
        }
    };
    if error.is_none() {
        error = roster.as_ref().and_then(|r| r.error.clone());
    }
    Survey {
        places: survey(here, roster.as_ref(), index.as_ref(), &recents),
        projects_dir,
        error,
    }
}

/// Merges the three sources into one row per checkout, best first.
///
/// Pure, so the merge and the order can be tested without a machine that has
/// sessions on it.
pub fn survey(
    here: Option<&Path>,
    roster: Option<&Roster>,
    index: Option<&SessionIndex>,
    recents: &[PathBuf],
) -> Vec<Place> {
    let mut found: BTreeMap<String, Place> = BTreeMap::new();

    // The repository the window is already about is always a row, even with no
    // sessions and no history: an operator who switched away has to be able to
    // switch back, and a screen that has dropped where they came from is a trap.
    if let Some(here) = here {
        entry(&mut found, here).here = true;
    }

    if let Some(roster) = roster {
        for session in roster.sessions.iter().filter(|s| s.is_live()) {
            let Some(cwd) = session.cwd.as_deref() else {
                continue;
            };
            let cwd = PathBuf::from(cwd);
            let root = checkout_containing(&cwd).unwrap_or(cwd);
            let place = entry(&mut found, &root);
            if session.activity == Activity::Working {
                place.working += 1;
            } else {
                place.idle += 1;
            }
            place.last = place
                .last
                .max(session.modified.map(WallTime::from_system_time));
        }
    }

    if let Some(index) = index {
        for session in &index.sessions {
            let Some(repo) = session.repo.as_deref() else {
                continue;
            };
            let root = checkout_containing(repo).unwrap_or_else(|| repo.to_path_buf());
            let place = entry(&mut found, &root);
            place.sessions += 1;
            place.last = place
                .last
                .max(Some(WallTime::from_unix_millis(session.modified_ms)));
        }
    }

    for root in recents {
        entry(&mut found, root).recent = true;
    }

    let mut places: Vec<Place> = found.into_values().collect();
    places.sort_by(order);
    places
}

/// The row for `root`, created on first sight with the filesystem read once.
fn entry<'a>(found: &'a mut BTreeMap<String, Place>, root: &Path) -> &'a mut Place {
    found.entry(key(root)).or_insert_with(|| {
        let exists = root.is_dir();
        Place {
            root: root.to_path_buf(),
            working: 0,
            idle: 0,
            sessions: 0,
            last: None,
            exists,
            git: exists && root.join(".git").exists(),
            here: false,
            recent: false,
        }
    })
}

/// The order the rows are offered in.
///
/// Deliberately not "most recent first". The question this screen answers is
/// *where is work happening*, so a repository with an agent working in it
/// outranks one that was busy yesterday, however recently — and the repository
/// the window is already about stays at the top, so switching back is the same
/// gesture as switching away.
fn order(a: &Place, b: &Place) -> std::cmp::Ordering {
    b.here
        .cmp(&a.here)
        // Above the live counts, and only just: an agent whose `cwd` is a plain
        // folder rather than a checkout is real and is worth a row, but it is
        // not somewhere Polis can go, and it was sitting second on this machine
        // — above three repositories the operator could actually have opened.
        .then_with(|| b.openable().cmp(&a.openable()))
        .then_with(|| b.working.cmp(&a.working))
        .then_with(|| b.idle.cmp(&a.idle))
        .then_with(|| b.last.cmp(&a.last))
        .then_with(|| b.recent.cmp(&a.recent))
        .then_with(|| b.sessions.cmp(&a.sessions))
        .then_with(|| a.root.cmp(&b.root))
}

// ---------------------------------------------------------------------------
// Recents
// ---------------------------------------------------------------------------

/// The checkouts Polis has opened, most recent first.
///
/// A missing or malformed file is an empty list and never an error — the rule
/// [`crate::config::Config::load`] follows, because a launcher that refuses to
/// open over a corrupt convenience file is worse than one with a short list.
pub fn recent(state_dir: Option<&Path>) -> Vec<PathBuf> {
    let Some(path) = state_dir.map(|dir| dir.join(RECENTS_FILE)) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<PathBuf>>(&text).unwrap_or_default()
}

/// Puts `root` at the front of the recents, keeping at most [`RECENTS`].
///
/// Best effort in both directions: a state directory that cannot be created or
/// written is a machine that shows a shorter list, which is not a reason to fail
/// to open a map.
pub fn remember(state_dir: Option<&Path>, root: &Path) {
    let Some(dir) = state_dir else {
        return;
    };
    let mut list = recent(Some(dir));
    let wanted = key(root);
    list.retain(|p| key(p) != wanted);
    list.insert(0, root.to_path_buf());
    list.truncate(RECENTS);
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    if let Ok(text) = serde_json::to_string_pretty(&list) {
        let _ = std::fs::write(dir.join(RECENTS_FILE), text);
    }
}

// ---------------------------------------------------------------------------
// The launcher
// ---------------------------------------------------------------------------

/// What the operator did with the launcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Watch this checkout. The caller decides what "watch" means for the
    /// window it is: a live watch stays live, a map stays a map.
    Open(PathBuf),
    /// Leave the launcher and go back to what was underneath it. Only ever
    /// returned when there *is* something underneath — see [`Launcher::over`].
    Dismiss,
}

/// The repository launcher: the front door, and the switcher.
#[derive(Debug)]
pub struct Launcher {
    /// The last completed scan. `None` until the first one lands.
    survey: Option<Survey>,
    /// A scan in flight. The previous list stays on screen while it runs.
    pending: Option<Receiver<Survey>>,
    /// When the next rescan is due.
    next: Instant,
    /// The repository the window is currently about, when it has one.
    here: Option<PathBuf>,
    /// Where the recents are read from and written to.
    state_dir: Option<PathBuf>,
    /// The label of the map underneath, when the launcher is over one. `None`
    /// makes this the front door, which has nothing to go back to.
    over: Option<String>,
    filter: String,
    /// A path typed by hand, for a checkout no source knows about yet.
    typed: String,
    /// Why the typed path was refused, until it changes.
    typed_error: Option<String>,
    cursor: usize,
    scroll_to_cursor: bool,
    focused: bool,
}

impl Launcher {
    /// Opens the launcher and starts the first scan.
    ///
    /// `over` is the label of the map it is covering — `None` for bare `polis`,
    /// where there is nothing behind it and therefore nothing to dismiss to.
    pub fn start(here: Option<PathBuf>, state_dir: Option<PathBuf>, over: Option<String>) -> Self {
        let mut launcher = Self {
            survey: None,
            pending: None,
            next: Instant::now(),
            here,
            state_dir,
            over,
            filter: String::new(),
            typed: String::new(),
            typed_error: None,
            cursor: 0,
            scroll_to_cursor: false,
            focused: false,
        };
        launcher.rescan();
        launcher
    }

    /// Starts a scan on its own thread, unless one is already running.
    fn rescan(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let here = self.here.clone();
        let state_dir = self.state_dir.clone();
        if std::thread::Builder::new()
            .name("polis-repo-scan".to_owned())
            .spawn(move || {
                let _ = tx.send(scan(here.as_deref(), state_dir.as_deref()));
            })
            .is_ok()
        {
            self.pending = Some(rx);
        }
        self.next = Instant::now() + RESCAN;
    }

    /// Collects a finished scan and starts the next one when it is due.
    fn poll(&mut self) {
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(survey) => {
                    self.survey = Some(survey);
                    self.pending = None;
                }
                // A scan thread that panicked drops the sender. That is a list
                // that did not refresh, never a crash — the rule the four ingest
                // channels follow.
                Err(TryRecvError::Disconnected) => self.pending = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.pending.is_none() && Instant::now() >= self.next {
            self.rescan();
        }
    }

    /// How long the window may sleep before this screen wants another frame.
    ///
    /// Two answers, because they are two different urgencies: a scan in flight
    /// wants [`POLL`] so the list appears the moment it lands, and a list on
    /// screen wants only the time left until the next rescan.
    pub fn wake_after(&self) -> Duration {
        if self.pending.is_some() {
            return POLL;
        }
        self.next.saturating_duration_since(Instant::now())
    }

    /// The repository the window is about, for the caller's own chrome.
    pub fn here(&self) -> Option<&Path> {
        self.here.as_deref()
    }

    /// Draws the launcher. Returns what the operator chose, if anything.
    pub fn draw(&mut self, ui: &mut egui::Ui) -> Option<Choice> {
        self.poll();
        let (nav, mut chosen) = self.draw_head(ui);
        let list = self.draw_list(ui, nav);
        if chosen.is_none() {
            chosen = list;
        }
        chosen
    }

    /// The banner, the filter and the way back. Returns the frame's list
    /// navigation, because it has to be read before any text field claims it.
    fn draw_head(&mut self, ui: &mut egui::Ui) -> (Nav, Option<Choice>) {
        let mut chosen = None;

        // Left-aligned, on the same margin as the columns under it. Centring
        // it put the heading against whatever rect the widest row had grown the
        // parent to, which on a machine with 90-character paths in `Temp` was
        // half a screen to the right with the line under it cut off.
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(
                RichText::new("POLIS")
                    .size(24.0)
                    .color(palette::selection().color())
                    .monospace(),
            );
            ui.label(
                RichText::new("pick a repository to watch — every agent working in it, live")
                    .color(palette::district_label().color()),
            );
        });
        ui.add_space(8.0);

        // Read before any text field is drawn, so the arrow keys are still
        // unclaimed: a widget with the keyboard consumes what it uses during its
        // own pass, and the list has to be navigable while the operator types.
        let nav = Nav::read(ui.ctx());
        let escape = ui.ctx().input(|i| i.key_pressed(egui::Key::Escape));

        ui.horizontal(|ui| {
            ui.label(RichText::new("filter").color(palette::worker().color()));
            let box_ = ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("repository name or path")
                    .desired_width(300.0),
            );
            // The keyboard starts in the filter box, so "type to filter" is true
            // without a click first. Requested once: taking focus back every
            // frame would fight the operator clicking anything else.
            if !self.focused {
                box_.request_focus();
                self.focused = true;
            }
            if let Some(over) = self.over.clone() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(format!("keep watching {over}  (esc)"))
                        .on_hover_text("leave everything as it is and go back to the map")
                        .clicked()
                    {
                        chosen = Some(Choice::Dismiss);
                    }
                });
            }
        });
        ui.label(
            RichText::new(
                "agents working here first · arrow keys move · enter opens · type to filter \
                 · or click any row",
            )
            .small()
            .color(palette::worker().color()),
        );
        ui.add_space(6.0);

        if let Some(typed) = self.draw_open_row(ui) {
            chosen = Some(typed);
        }
        ui.add_space(6.0);

        // Escape is a way back and never a way out: the front door has nothing
        // behind it, and closing the window is what the window manager is for.
        if escape && self.over.is_some() {
            chosen = Some(Choice::Dismiss);
        }
        (nav, chosen)
    }

    /// The list itself, and the three things that are not a list: a first scan
    /// still running, a machine with no repositories, and a filter that matches
    /// nothing.
    fn draw_list(&mut self, ui: &mut egui::Ui, nav: Nav) -> Option<Choice> {
        let mut chosen = None;
        let Some(survey) = self.survey.as_ref() else {
            ui.add_space(20.0);
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                ui.spinner();
                ui.label(
                    RichText::new("looking for repositories with agents in them")
                        .color(palette::worker().color()),
                );
            });
            return chosen;
        };

        if let Some(error) = &survey.error {
            ui.label(
                RichText::new(error.clone())
                    .small()
                    .color(palette::contention().color()),
            );
        }

        header(ui);
        ui.separator();

        let needle = self.filter.to_lowercase();
        let rows: Vec<&Place> = survey
            .places
            .iter()
            .filter(|p| needle.is_empty() || matches(p, &needle))
            .collect();

        if rows.is_empty() {
            ui.add_space(18.0);
            ui.horizontal_wrapped(|ui| {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(if survey.places.is_empty() {
                        "No repository has been worked in on this machine yet. Type a path \
                         below, or run `claude` in any checkout and it appears here."
                    } else {
                        "Nothing matches that filter."
                    })
                    .color(palette::worker().color()),
                );
            });
            return chosen;
        }

        // The cursor is an index into what is on screen, and filtering changes
        // what that is. Clamping here rather than when the filter changes keeps
        // one rule in one place.
        let moved = nav.apply(&mut self.cursor, rows.len());
        self.scroll_to_cursor |= moved;
        if nav.open {
            if let Some(place) = rows.get(self.cursor).filter(|p| p.openable()) {
                chosen = Some(Choice::Open(place.root.clone()));
            }
        }

        // A row opens on a **pointer** click. egui also delivers `enter` as a
        // click to whatever widget holds keyboard focus, and `enter` is already
        // this list's own key — without this, `enter` on the third row opened
        // the first one, which is the kind of bug an operator reads as the
        // launcher having a mind of its own.
        let pointer_click = ui.ctx().input(|i| i.pointer.any_click());
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (i, place) in rows.iter().enumerate() {
                    let at_cursor = i == self.cursor;
                    let response = row(ui, place, at_cursor);
                    if at_cursor && std::mem::take(&mut self.scroll_to_cursor) {
                        response.scroll_to_me(Some(egui::Align::Center));
                    }
                    if pointer_click && response.clicked() && place.openable() {
                        chosen = Some(Choice::Open(place.root.clone()));
                        self.cursor = i;
                    }
                }
            });
        chosen
    }

    /// The "some other folder" row: a checkout no session has ever run in.
    ///
    /// A row rather than a panel pinned to the bottom edge. The panel was
    /// correct and invisible — on a window sized smaller than the layout it was
    /// drawn for, it sat below the fold, and a control nobody can see is a
    /// control that does not exist.
    ///
    /// A path that is *inside* a checkout opens the checkout, rather than
    /// refusing something the operator meant perfectly clearly: `polis` is
    /// routinely started from `repo/crates/thing`, and so is `claude`.
    fn draw_open_row(&mut self, ui: &mut egui::Ui) -> Option<Choice> {
        let mut chosen = None;
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(RichText::new("or open").color(palette::worker().color()));
            let box_ = ui.add(
                egui::TextEdit::singleline(&mut self.typed)
                    .hint_text("path to any git checkout")
                    .desired_width(360.0),
            );
            if box_.changed() {
                self.typed_error = None;
            }
            let entered = box_.lost_focus() && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter));
            if (entered || ui.button("open").clicked()) && !self.typed.trim().is_empty() {
                match resolve(self.typed.trim()) {
                    Ok(root) => chosen = Some(Choice::Open(root)),
                    Err(error) => self.typed_error = Some(error),
                }
            }
            if let Some(error) = self.typed_error.clone() {
                ui.label(
                    RichText::new(error)
                        .small()
                        .color(palette::contention().color()),
                );
            }
        });
        chosen
    }
}

/// What a typed path resolves to, or why it does not.
fn resolve(typed: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(typed);
    if !path.is_dir() {
        return Err(format!("there is no folder at {}", path.display()));
    }
    let canonical = path.canonicalize().unwrap_or(path);
    let canonical = crate::cli::strip_verbatim(canonical);
    checkout_containing(&canonical).ok_or_else(|| {
        format!(
            "{} is not a git checkout and is not inside one — Polis builds the city out of \
             git history, so there is nothing to draw without it",
            canonical.display()
        )
    })
}

/// Whether a row survives the filter.
fn matches(place: &Place, needle: &str) -> bool {
    place.name().to_lowercase().contains(needle)
        || place
            .root
            .display()
            .to_string()
            .to_lowercase()
            .contains(needle)
}

/// The live column: wide enough for `12 working, 3 idle`.
const AGENTS_COLUMN: usize = 22;
/// The repository name.
const NAME_COLUMN: usize = 22;
/// A timestamp, as [`crate::format::wall_time`] writes one.
const TIME_COLUMN: usize = 17;
/// The path.
///
/// Fixed, and every row held to it by [`elide`], because a row that does not fit
/// **widens the screen it is drawn on**: egui grows a parent to whatever its
/// children ask for, and one 96-character path under `AppData\Local\Temp` was
/// enough to push the heading and the filter box off the right-hand edge.
const PATH_COLUMN: usize = 52;

/// `text`, cut to `budget` characters with an ellipsis when it is longer.
fn clip(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_owned();
    }
    let kept: String = text.chars().take(budget.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// A path in `budget` characters, cut in the **middle**.
///
/// The two ends are the informative ones — the drive or home directory, and the
/// checkout's own name — and a path cut at the right loses the second, which is
/// the half an operator actually reads.
fn elide(path: &Path, budget: usize) -> String {
    let text = path.display().to_string();
    let len = text.chars().count();
    if len <= budget {
        return text;
    }
    let tail = budget * 2 / 3;
    let head = budget.saturating_sub(tail + 1);
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(len - tail).collect();
    format!("{start}…{end}")
}

/// The column headings, in the order the list is sorted by.
fn header(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        for (text, width) in [
            ("agents", AGENTS_COLUMN),
            ("repository", NAME_COLUMN),
            ("last active", TIME_COLUMN),
            ("where", 0),
        ] {
            let label = if width > 0 {
                format!("{text:<width$}")
            } else {
                text.to_owned()
            };
            ui.label(
                RichText::new(label)
                    .monospace()
                    .color(palette::district_label().color()),
            );
        }
    });
}

/// One repository as a row. Returns the row's own click response.
fn row(ui: &mut egui::Ui, place: &Place, at_cursor: bool) -> egui::Response {
    let openable = place.openable();
    let response = ui
        .horizontal(|ui| {
            ui.add_space(4.0);
            // The live column first, because it is what the screen is for and
            // what the list is sorted by.
            let agents = place.agents();
            ui.label(
                RichText::new(format!("{agents:<AGENTS_COLUMN$}"))
                    .monospace()
                    .color(if place.working > 0 {
                        palette::status(polis_world::ThreadStatus::Working).color()
                    } else if place.idle > 0 {
                        palette::status(polis_world::ThreadStatus::Idle).color()
                    } else {
                        palette::district_label().color()
                    }),
            );

            let name = clip(&place.name(), NAME_COLUMN);
            ui.label(
                RichText::new(format!("{name:<NAME_COLUMN$}"))
                    .monospace()
                    .strong()
                    .color(if openable {
                        palette::selection().color()
                    } else {
                        palette::status(polis_world::ThreadStatus::Idle).color()
                    }),
            );

            let when = place
                .last
                .map_or_else(|| "—".to_owned(), crate::format::wall_time);
            ui.label(
                RichText::new(format!("{when:<TIME_COLUMN$}"))
                    .monospace()
                    .color(palette::worker().color()),
            );

            ui.label(
                RichText::new(format!("{:<PATH_COLUMN$}", elide(&place.root, PATH_COLUMN)))
                    .monospace()
                    .small()
                    .color(palette::district_label().color()),
            );

            if place.here {
                ui.label(
                    RichText::new("· watching now")
                        .small()
                        .color(palette::hover().color()),
                );
            }
            if let Some(refusal) = place.refusal() {
                ui.label(
                    RichText::new(format!("· {refusal}"))
                        .small()
                        .color(palette::contention().color()),
                );
            } else if place.sessions > 0 && place.live() == 0 {
                ui.label(
                    RichText::new(format!("· {} past sessions", place.sessions))
                        .small()
                        .color(palette::district_label().color()),
                );
            }
        })
        .response;
    let rect = response.rect;
    let hit = ui.interact(
        rect,
        ui.id().with(("polis-repo-row", place.root.clone())),
        egui::Sense::click(),
    );
    if at_cursor || hit.hovered() {
        ui.painter().rect_stroke(
            rect.expand2(egui::vec2(2.0, 1.0)),
            2.0,
            egui::Stroke::new(
                1.0,
                if at_cursor {
                    palette::selection().color()
                } else {
                    palette::hover().color()
                },
            ),
            egui::StrokeKind::Inside,
        );
    }
    hit
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use polis_ingest::live::{Fit, LiveSession};

    use super::*;

    fn session(cwd: &str, activity: Activity) -> LiveSession {
        LiveSession {
            session: polis_events::SessionId::new("s"),
            session_dir: PathBuf::from("/p/s"),
            transcript: PathBuf::from("/p/s.jsonl"),
            project_dir: PathBuf::from("/p"),
            cwd: Some(cwd.to_owned()),
            fit: Fit::Inside,
            bytes: 1,
            modified: None,
            quiet_for: None,
            activity,
            subagents: 0,
            superseded_by: None,
            tailing: true,
            started_while_watching: false,
        }
    }

    fn roster(sessions: Vec<LiveSession>) -> Roster {
        let mut roster = Roster::empty(Path::new("/p"), None);
        roster.sessions = sessions;
        roster
    }

    /// The checkout is found by walking up, and a worktree's `.git` **file**
    /// counts: PRD §7.6's second checkout of the same repository is a repository
    /// in its own right here, not a miss.
    #[test]
    fn the_checkout_is_the_nearest_ancestor_holding_a_dot_git() {
        let dir = crate::testutil::scratch("repos-walk-up");
        let repo = dir.join("repo");
        let deep = repo.join("crates").join("thing");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert_eq!(checkout_containing(&deep), Some(repo.clone()));

        let worktree = dir.join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: ../repo/.git/worktrees/wt").unwrap();
        assert_eq!(checkout_containing(&worktree), Some(worktree));

        let loose = dir.join("loose");
        std::fs::create_dir_all(&loose).unwrap();
        assert_eq!(checkout_containing(&loose), None);
    }

    /// Two agents started in two subdirectories of one repository are one row
    /// with two agents on it, not two rows with one each.
    #[test]
    fn sessions_in_subdirectories_merge_into_one_repository_row() {
        let dir = crate::testutil::scratch("repos-merge");
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("crates")).unwrap();
        let roster = roster(vec![
            session(
                &repo.join("crates").display().to_string(),
                Activity::Working,
            ),
            session(&repo.display().to_string(), Activity::Idle),
        ]);

        let places = survey(None, Some(&roster), None, &[]);
        assert_eq!(places.len(), 1, "{places:?}");
        assert_eq!(places[0].root, repo);
        assert_eq!(places[0].working, 1);
        assert_eq!(places[0].idle, 1);
        assert_eq!(places[0].agents(), "1 working, 1 idle");
    }

    /// A dormant session is history, not a live agent, and must not be counted
    /// as one — the column would then say "idle" about a machine with nothing
    /// running on it at all.
    #[test]
    fn a_dormant_session_is_not_a_live_agent() {
        let dir = crate::testutil::scratch("repos-dormant");
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let roster = roster(vec![session(
            &repo.display().to_string(),
            Activity::Dormant,
        )]);
        let places = survey(None, Some(&roster), None, &[]);
        assert!(places.is_empty(), "{places:?}");
    }

    /// Where work is happening outranks where work happened, and the repository
    /// the window is already about stays at the top so switching back is the
    /// same gesture as switching away.
    #[test]
    fn the_order_is_working_agents_first_and_the_current_repo_above_everything() {
        let dir = crate::testutil::scratch("repos-order");
        let mut roots = Vec::new();
        for name in ["here", "busy", "quiet"] {
            let root = dir.join(name);
            std::fs::create_dir_all(root.join(".git")).unwrap();
            roots.push(root);
        }
        let busy = roster(vec![
            session(&roots[1].display().to_string(), Activity::Working),
            session(&roots[2].display().to_string(), Activity::Idle),
        ]);
        let places = survey(Some(&roots[0]), Some(&busy), None, &[]);
        let names: Vec<String> = places.iter().map(Place::name).collect();
        assert_eq!(names, vec!["here", "busy", "quiet"], "{places:?}");
        assert!(places[0].here);

        // An agent working in a folder that is not a checkout is a row, and it
        // is a row *below* every repository that can actually be opened —
        // measured on this machine, where a session started in `C:\coding` sat
        // second, above three checkouts with history in them.
        let loose = dir.join("loose");
        std::fs::create_dir_all(&loose).unwrap();
        let mixed = roster(vec![
            session(&loose.display().to_string(), Activity::Working),
            session(&roots[2].display().to_string(), Activity::Idle),
        ]);
        let places = survey(None, Some(&mixed), None, &[]);
        let names: Vec<String> = places.iter().map(Place::name).collect();
        assert_eq!(names, vec!["quiet", "loose"], "{places:?}");
    }

    /// A repository whose folder is gone is offered and refused, with the reason
    /// on the row — never silently dropped, because an operator who remembers
    /// working there needs to know which of the two happened.
    #[test]
    fn a_repository_that_is_gone_is_shown_with_its_reason_and_cannot_be_opened() {
        let dir = crate::testutil::scratch("repos-gone");
        let gone = dir.join("deleted");
        let places = survey(None, None, None, std::slice::from_ref(&gone));
        assert_eq!(places.len(), 1);
        assert!(!places[0].openable());
        assert_eq!(places[0].refusal(), Some("the folder is gone"));

        let plain = dir.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let places = survey(None, None, None, &[plain]);
        assert_eq!(places[0].refusal(), Some("not a git checkout"));
    }

    /// The recents are a most-recent-first list with no duplicates, and reading
    /// one back is what the launcher shows.
    #[test]
    fn the_recents_are_most_recent_first_and_hold_each_checkout_once() {
        let dir = crate::testutil::scratch("repos-recents");
        let state = dir.join("state");
        let a = dir.join("a");
        let b = dir.join("b");
        remember(Some(&state), &a);
        remember(Some(&state), &b);
        remember(Some(&state), &a);
        assert_eq!(recent(Some(&state)), vec![a, b]);

        // A file that is not there, and a file that is nonsense, are both an
        // empty list rather than a failure to open the launcher at all.
        assert!(recent(Some(&dir.join("nothing"))).is_empty());
        std::fs::write(state.join(RECENTS_FILE), "{ not json").unwrap();
        assert!(recent(Some(&state)).is_empty());
    }

    fn place(root: &Path, working: usize) -> Place {
        Place {
            root: root.to_path_buf(),
            working,
            idle: 0,
            sessions: 0,
            last: None,
            exists: true,
            git: true,
            here: false,
            recent: false,
        }
    }

    fn launcher_over(places: Vec<Place>, over: Option<&str>) -> Launcher {
        Launcher {
            survey: Some(Survey {
                places,
                projects_dir: Some(PathBuf::from("/p")),
                error: None,
            }),
            pending: None,
            // Far enough out that a headless pass never starts a real scan.
            next: Instant::now() + Duration::from_secs(3_600),
            here: None,
            state_dir: None,
            over: over.map(ToOwned::to_owned),
            filter: String::new(),
            typed: String::new(),
            typed_error: None,
            cursor: 0,
            scroll_to_cursor: false,
            focused: false,
        }
    }

    /// One headless pass with `keys` pressed. Returns what the launcher chose.
    fn pass(launcher: &mut Launcher, ctx: &egui::Context, keys: &[egui::Key]) -> Option<Choice> {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::new(1200.0, 800.0));
        let mut input = egui::RawInput {
            screen_rect: Some(rect),
            ..Default::default()
        };
        input
            .viewports
            .entry(input.viewport_id)
            .or_default()
            .inner_rect = Some(rect);
        for key in keys {
            input.events.push(egui::Event::Key {
                key: *key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            });
        }
        let mut chosen = None;
        let mut full = ctx.run_ui(input, |ui| {
            chosen = launcher.draw(ui);
        });
        // `epaint` panics if a texture delta is dropped unapplied, and there is
        // no painter here to apply it to.
        full.textures_delta.clear();
        chosen
    }

    /// The launcher is the front door, so it has to work with no pointer at all:
    /// arrow keys move, enter opens the row the cursor is on.
    #[test]
    fn arrow_keys_move_the_cursor_and_enter_opens_the_repository_it_is_on() {
        let ctx = egui::Context::default();
        let one = PathBuf::from("/r/one");
        let two = PathBuf::from("/r/two");
        let mut launcher = launcher_over(vec![place(&one, 2), place(&two, 1)], None);

        assert!(pass(&mut launcher, &ctx, &[]).is_none());
        assert_eq!(launcher.cursor, 0);
        assert!(pass(&mut launcher, &ctx, &[egui::Key::ArrowDown]).is_none());
        assert_eq!(launcher.cursor, 1);
        assert_eq!(
            pass(&mut launcher, &ctx, &[egui::Key::Enter]),
            Some(Choice::Open(two))
        );
    }

    /// Enter on a repository that cannot be opened does nothing, rather than
    /// tearing down a working watch for a city that will fail to generate.
    #[test]
    fn enter_on_a_repository_that_is_gone_opens_nothing() {
        let ctx = egui::Context::default();
        let mut gone = place(Path::new("/gone"), 0);
        gone.exists = false;
        gone.git = false;
        let mut launcher = launcher_over(vec![gone], None);
        assert!(pass(&mut launcher, &ctx, &[]).is_none());
        assert!(pass(&mut launcher, &ctx, &[egui::Key::Enter]).is_none());
    }

    /// Escape is a way *back*, so it exists exactly when there is something to
    /// go back to. On the front door — bare `polis`, nothing behind it — it must
    /// not report a dismissal the caller has no way to honour.
    #[test]
    fn escape_leaves_the_launcher_only_when_a_map_is_underneath_it() {
        let ctx = egui::Context::default();
        let here = PathBuf::from("/r/here");

        let mut over_a_map = launcher_over(vec![place(&here, 0)], Some("AGENTOLIS"));
        assert!(pass(&mut over_a_map, &ctx, &[]).is_none());
        assert_eq!(
            pass(&mut over_a_map, &ctx, &[egui::Key::Escape]),
            Some(Choice::Dismiss)
        );

        let mut front_door = launcher_over(vec![place(&here, 0)], None);
        assert!(pass(&mut front_door, &ctx, &[]).is_none());
        assert!(pass(&mut front_door, &ctx, &[egui::Key::Escape]).is_none());
    }

    /// A scan in flight wants a frame in 80 ms; a list on screen wants only the
    /// time left until the next rescan. One reason, two intervals — and getting
    /// this wrong spends twenty-five frames a second on a screen that changes
    /// twice (PRD §13.1).
    #[test]
    fn the_wake_interval_is_the_scan_when_one_is_running_and_the_rescan_otherwise() {
        let mut launcher = launcher_over(Vec::new(), None);
        let quiet = launcher.wake_after();
        assert!(quiet > POLL, "{quiet:?}");

        let (_tx, rx) = crossbeam_channel::bounded::<Survey>(1);
        launcher.pending = Some(rx);
        assert_eq!(launcher.wake_after(), POLL);
    }

    /// A typed path inside a checkout opens the checkout, and a folder that is
    /// not in one is refused with a sentence rather than accepted and then
    /// failed on.
    #[test]
    fn a_typed_path_resolves_to_the_checkout_it_is_inside() {
        let dir = crate::testutil::scratch("repos-typed");
        let repo = dir.join("repo");
        let deep = repo.join("src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let resolved = resolve(&deep.display().to_string()).unwrap();
        assert!(resolved.ends_with("repo"), "{resolved:?}");

        let loose = dir.join("loose");
        std::fs::create_dir_all(&loose).unwrap();
        let error = resolve(&loose.display().to_string()).unwrap_err();
        assert!(error.contains("not a git checkout"), "{error}");
        let error = resolve(&dir.join("absent").display().to_string()).unwrap_err();
        assert!(error.contains("no folder"), "{error}");
    }
}
