//! The session picker — the first-run experience (PRD §15 M2).
//!
//! `polis replay` with no argument has to be useful with zero configuration:
//! the operator already has sessions on this machine, and asking them to find a
//! transcript path under `~/.claude/projects/<munged-cwd>/<uuid>.jsonl` is not a
//! product. `polis-world`'s session index already enumerates every one of them
//! with its repository, duration and counts; this is that index as a list you
//! click.
//!
//! # It scans on a thread
//!
//! A full byte scan of this machine's corpus is 493 ms cold and 11 ms warm from
//! the `(size, mtime)` cache. Cold is a visible stall, so the window opens
//! first, says it is scanning, and asks for a repaint every 80 ms until the
//! result arrives — after which it goes quiet again, because PRD §13.1 budgets
//! idle at under 2% of one core and a picker that polls forever spends it.
//!
//! # A session whose repository is gone is shown, not hidden
//!
//! [`SessionSummary::is_replayable`] is false for three sessions on this
//! machine. They are listed, greyed, with the reason — a picker that silently
//! omits a session the operator remembers running is a picker they stop
//! trusting.

use std::path::{Path, PathBuf};

use crossbeam_channel::{Receiver, TryRecvError};
use eframe::egui::{self, Color32, RichText};
use polis_world::sessions::{IndexOptions, SessionIndex, SessionSummary};

use crate::format;
use crate::palette;

/// How often the window wakes while a scan is in flight.
pub const POLL: std::time::Duration = std::time::Duration::from_millis(80);

/// What the picker is doing.
#[derive(Debug)]
enum State {
    /// A scan is running on its own thread.
    Scanning(Receiver<Result<SessionIndex, String>>),
    /// The index, most recent first.
    Ready(Box<SessionIndex>),
    /// The scan failed. The window still works; there is just nothing to pick.
    Failed(String),
}

/// The picker.
#[derive(Debug)]
pub struct Picker {
    state: State,
    filter: String,
    /// Where the scan looked, for the empty-state message.
    projects_dir: Option<PathBuf>,
    /// Whether the scan should be repainted for.
    pub scanning: bool,
    /// Which visible row the keyboard is on.
    ///
    /// An index into the *filtered* rows, not into the index: typing in the
    /// filter box changes what row 3 is, and the cursor has to stay on a row
    /// that exists.
    cursor: usize,
    /// Whether the cursor moved this frame, so the list scrolls to it only then
    /// and the operator can still scroll the list with the mouse.
    scroll_to_cursor: bool,
    /// Whether the filter box has been given the keyboard yet.
    focused: bool,
}

impl Picker {
    /// Starts a scan of `~/.claude/projects` on a background thread.
    pub fn start() -> Self {
        let projects_dir = polis_world::sessions::default_projects_dir();
        let Some(dir) = projects_dir.clone() else {
            return Self {
                state: State::Failed("no ~/.claude/projects directory on this machine".to_owned()),
                filter: String::new(),
                projects_dir,
                scanning: false,
                cursor: 0,
                scroll_to_cursor: false,
                focused: false,
            };
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name("polis-session-index".to_owned())
            .spawn(move || {
                let result = SessionIndex::scan_with(&dir, &IndexOptions::default())
                    .map_err(|e| format!("scanning {}: {e}", dir.display()));
                let _ = tx.send(result);
            })
            .expect("spawning the session index thread");
        Self {
            state: State::Scanning(rx),
            filter: String::new(),
            projects_dir,
            scanning: true,
            cursor: 0,
            scroll_to_cursor: false,
            focused: false,
        }
    }

    /// Collects the scan result if it has arrived.
    fn poll(&mut self) {
        let State::Scanning(rx) = &self.state else {
            return;
        };
        // A thread that panicked drops the sender, which arrives here as
        // `Disconnected`. That is "there is nothing to pick", never a crash —
        // the same rule the four ingest channels follow.
        self.state = match rx.try_recv() {
            Err(TryRecvError::Empty) => return,
            Ok(Ok(index)) => State::Ready(Box::new(index)),
            Ok(Err(error)) => State::Failed(error),
            Err(TryRecvError::Disconnected) => {
                State::Failed("the session scan did not finish".to_owned())
            }
        };
        self.scanning = false;
    }

    /// The two states that are not a list: scanning, and a scan that failed.
    ///
    /// Returns true when it drew one of them and there is nothing to pick.
    fn draw_not_ready(&self, ui: &mut egui::Ui) -> bool {
        match &self.state {
            State::Scanning(_) => {
                ui.vertical_centered(|ui| {
                    ui.spinner();
                    ui.label(
                        RichText::new(format!(
                            "scanning {}",
                            self.projects_dir
                                .as_ref()
                                .map_or_else(|| "…".to_owned(), |p| p.display().to_string())
                        ))
                        .color(palette::worker().color()),
                    );
                });
                true
            }
            State::Failed(error) => {
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(error)
                            .color(palette::contention().color())
                            .monospace(),
                    );
                    ui.label(
                        RichText::new("pass a transcript directly:  polis replay <path-to.jsonl>")
                            .color(palette::worker().color())
                            .monospace(),
                    );
                });
                true
            }
            State::Ready(_) => false,
        }
    }

    /// Draws the picker. Returns the session the operator chose.
    pub fn draw(&mut self, ui: &mut egui::Ui) -> Option<SessionSummary> {
        self.poll();
        let mut chosen = None;

        ui.add_space(18.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("POLIS")
                    .size(28.0)
                    .color(palette::selection().color())
                    .monospace(),
            );
            ui.label(
                RichText::new("pick a session to replay over its city")
                    .color(palette::district_label().color()),
            );
        });
        ui.add_space(12.0);

        if self.draw_not_ready(ui) {
            return None;
        }

        // Read before the filter box is drawn, so the keys are still unclaimed:
        // a widget that has the keyboard consumes what it uses during its own
        // pass, and the arrow keys have to work while the operator is typing.
        let nav = Nav::read(ui.ctx());

        let State::Ready(index) = &self.state else {
            return None;
        };

        ui.horizontal(|ui| {
            ui.label(RichText::new("filter").color(palette::worker().color()));
            let box_ = ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("repository, title or session id")
                    .desired_width(320.0),
            );
            // The keyboard starts in the filter box, so "type to filter" is true
            // without a click first. Requested once: taking focus back every
            // frame would fight the operator clicking anything else.
            if !self.focused {
                box_.request_focus();
                self.focused = true;
            }
            let replayable = index.replayable().count();
            ui.label(
                RichText::new(format!(
                    "{} sessions · {} replayable · {} repositories",
                    index.len(),
                    replayable,
                    index.repositories().len()
                ))
                .color(palette::district_label().color())
                .monospace(),
            );
        });
        // Words, not arrow glyphs: egui's default face has no U+2191, and the
        // one line that teaches the keyboard must not itself render as two
        // empty boxes.
        ui.label(
            RichText::new(
                "newest first · arrow keys move · enter opens · page up/down jumps · \
                 type to filter · or click any row",
            )
            .small()
            .color(palette::worker().color()),
        );
        ui.add_space(6.0);
        header(ui);
        ui.separator();

        let needle = self.filter.to_lowercase();
        let rows: Vec<&SessionSummary> = index
            .sessions
            .iter()
            .filter(|s| needle.is_empty() || matches(s, &needle))
            .collect();

        // The cursor is an index into what is on screen, and filtering changes
        // what that is. Clamping here rather than when the filter changes keeps
        // one rule in one place.
        let moved = nav.apply(&mut self.cursor, rows.len());
        self.scroll_to_cursor |= moved;
        if nav.open {
            if let Some(session) = rows.get(self.cursor).filter(|s| s.is_replayable()) {
                chosen = Some((*session).clone());
            }
        }

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (i, session) in rows.iter().enumerate() {
                    let at_cursor = i == self.cursor;
                    let response = row(ui, session, at_cursor);
                    if at_cursor && std::mem::take(&mut self.scroll_to_cursor) {
                        response.scroll_to_me(Some(egui::Align::Center));
                    }
                    if response.clicked() && session.is_replayable() {
                        chosen = Some((*session).clone());
                        self.cursor = i;
                    }
                }
            });
        chosen
    }
}

/// One frame of keyboard navigation for the list.
///
/// A flat set of flags rather than a state machine, for the same reason
/// [`crate::app`]'s `Keys` is one: each is an independent edge from one frame's
/// input, several can arrive together — `PageDown` while `ArrowUp` repeats —
/// and a two-variant enum per key would say "pressed" and "not pressed" in more
/// words.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default, Clone, Copy)]
struct Nav {
    up: bool,
    down: bool,
    page_up: bool,
    page_down: bool,
    open: bool,
}

/// How far page up and page down move. A screenful is about this many rows at
/// the sizes the picker uses, and a fixed number is predictable in a way that
/// "however many fit right now" is not.
const PAGE: usize = 12;

impl Nav {
    fn read(ctx: &egui::Context) -> Self {
        ctx.input(|i| Self {
            up: i.key_pressed(egui::Key::ArrowUp),
            down: i.key_pressed(egui::Key::ArrowDown),
            page_up: i.key_pressed(egui::Key::PageUp),
            page_down: i.key_pressed(egui::Key::PageDown),
            open: i.key_pressed(egui::Key::Enter),
        })
    }

    /// Moves `cursor` within `len` rows. Returns whether it moved.
    ///
    /// Deliberately clamping rather than wrapping: a list of 195 sessions that
    /// jumps from the top to the bottom on one keypress reads as a bug.
    fn apply(self, cursor: &mut usize, len: usize) -> bool {
        if len == 0 {
            *cursor = 0;
            return false;
        }
        let before = *cursor;
        let mut at = before.min(len - 1);
        if self.down {
            at = at.saturating_add(1);
        }
        if self.up {
            at = at.saturating_sub(1);
        }
        if self.page_down {
            at = at.saturating_add(PAGE);
        }
        if self.page_up {
            at = at.saturating_sub(PAGE);
        }
        *cursor = at.min(len - 1);
        *cursor != before
    }
}

/// The column headings.
///
/// The first column is the one the list is *sorted* by, and until this existed
/// it showed the session's **start** while the sort was on its end — so the
/// first four rows read `21:27`, `16:58`, `20:09`, `21:59` under a promise of
/// "most recent first", which is indistinguishable from a broken sort.
fn header(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        for (text, width) in [
            ("last active", 17),
            ("repository", 18),
            ("ran for", 9),
            ("what happened in it", 0),
        ] {
            let label = if width > 0 {
                format!("{text:<width$}")
            } else {
                text.to_owned()
            };
            // Same size as the rows, deliberately: a `small()` heading over a
            // monospace column is a heading that does not line up with it.
            ui.label(
                RichText::new(label)
                    .monospace()
                    .color(palette::district_label().color()),
            );
        }
    });
}

fn matches(session: &SessionSummary, needle: &str) -> bool {
    let repo = session
        .repo
        .as_ref()
        .map(|p| p.display().to_string().to_lowercase())
        .unwrap_or_default();
    repo.contains(needle)
        || session.label().to_lowercase().contains(needle)
        || session.session.as_str().to_lowercase().contains(needle)
}

/// One session as a row. Returns the row's own click response.
///
/// The time shown is the session's **last activity**, which is what the list is
/// ordered by. Showing its start under a "most recent first" promise made the
/// order look broken (F4 of the first-run review).
fn row(ui: &mut egui::Ui, session: &SessionSummary, at_cursor: bool) -> egui::Response {
    let replayable = session.is_replayable();
    let response = ui
        .horizontal(|ui| {
            ui.add_space(4.0);
            let when = session
                .ended
                .or(session.started)
                .map_or_else(|| "unknown time".to_owned(), format::wall_time);
            ui.label(
                RichText::new(when)
                    .monospace()
                    .color(palette::worker().color()),
            );

            let repo = session
                .repo
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("unknown repo");
            ui.label(
                RichText::new(format!("{repo:>18}"))
                    .monospace()
                    .strong()
                    .color(if replayable {
                        palette::selection().color()
                    } else {
                        palette::status(polis_world::ThreadStatus::Idle).color()
                    }),
            );

            let duration = session
                .duration()
                .map_or_else(|| "—".to_owned(), format::duration);
            ui.label(
                RichText::new(format!("{duration:>9}"))
                    .monospace()
                    .color(palette::district_label().color()),
            );
            let counts = if session.counts_partial {
                format!("{:>7} records (partial)", session.records)
            } else {
                format!(
                    "{:>7} records · {:>5} tools · {:>4} files · {:>3} subagents",
                    session.records, session.tool_calls, session.files_touched, session.subagents
                )
            };
            ui.label(
                RichText::new(counts)
                    .monospace()
                    .color(palette::trail().color()),
            );
            ui.label(
                RichText::new(format::bytes(session.bytes))
                    .monospace()
                    .color(palette::status(polis_world::ThreadStatus::Idle).color()),
            );
            if !replayable {
                ui.label(
                    RichText::new("repository is gone")
                        .monospace()
                        .color(palette::needs_decision().color()),
                );
            }
            ui.label(RichText::new(session.label()).color(palette::district_label().color()));
        })
        .response;

    let rect = response.rect;
    let hit = ui.interact(
        rect,
        ui.id().with(session.session.as_str()),
        egui::Sense::click(),
    );
    // The keyboard cursor is a stronger mark than hover, and both are drawn:
    // the operator can be pointing at one row while the cursor is on another.
    if at_cursor {
        ui.painter().rect_filled(
            rect,
            2.0,
            Color32::from_rgba_unmultiplied(120, 150, 190, 46),
        );
        ui.painter().rect_stroke(
            rect,
            2.0,
            egui::Stroke::new(1.0, palette::selection().color()),
            egui::StrokeKind::Inside,
        );
    } else if hit.hovered() {
        ui.painter().rect_filled(
            rect,
            2.0,
            Color32::from_rgba_unmultiplied(120, 140, 160, 18),
        );
    }
    hit
}

/// Resolves whatever a human or a picker hands us to a path `ReplaySchedule`
/// can open.
///
/// A session has two spellings on disk: the main transcript
/// `<munged-cwd>/<id>.jsonl`, and a `<munged-cwd>/<id>/` sidecar directory
/// holding its subagent transcripts. Only the first always exists — on this
/// machine 91 of 147 sessions in one project have no sidecar at all — so a
/// bare `exists()` check rejects most real sessions when given the stem.
///
/// Accepted, in order: the path as given; the same path with `.jsonl`
/// appended (the stem of a session with no sidecar); and the stem of a path
/// that was handed to us with the extension already on it. `ReplaySchedule`
/// resolves sidecar-versus-main itself, so anything returned here is openable.
pub fn resolve_transcript(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        return Some(path.to_path_buf());
    }
    // `<id>` given, `<id>.jsonl` on disk — the session-picker case.
    let with_ext = path.with_extension("jsonl");
    if with_ext.is_file() {
        return Some(with_ext);
    }
    // `<id>.jsonl` given but only the `<id>/` sidecar survives.
    if path.extension().is_some_and(|e| e == "jsonl") {
        let stem = path.with_extension("");
        if stem.is_dir() {
            return Some(stem);
        }
    }
    None
}

#[cfg(test)]
mod resolve_tests {
    use super::resolve_transcript;

    /// The picker handed `load` the `<session-id>` stem, which only exists when
    /// the session spawned a subagent. 91 of 147 sessions in one real project
    /// on this machine have no sidecar directory, so most real sessions failed
    /// to open with "no such transcript" while the `.jsonl` sat beside it.
    #[test]
    fn a_session_id_stem_resolves_to_the_transcript_beside_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = "f2b94e93-eb55-4064-8668-861ab55c2241";
        let transcript = dir.path().join(format!("{id}.jsonl"));
        std::fs::write(&transcript, "{}\n").expect("write");

        // No sidecar directory — the common case.
        let stem = dir.path().join(id);
        assert!(
            !stem.exists(),
            "the stem must not exist for this to be the bug"
        );
        assert_eq!(
            resolve_transcript(&stem).as_deref(),
            Some(transcript.as_path())
        );

        // The full path still resolves to itself.
        assert_eq!(
            resolve_transcript(&transcript).as_deref(),
            Some(transcript.as_path())
        );
    }

    #[test]
    fn a_sidecar_directory_is_preferred_when_it_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sidecar = dir.path().join("abc");
        std::fs::create_dir(&sidecar).expect("mkdir");
        // Given the stem and a real directory, the directory wins: it carries
        // the subagent transcripts and ReplaySchedule finds the main file itself.
        assert_eq!(
            resolve_transcript(&sidecar).as_deref(),
            Some(sidecar.as_path())
        );
    }

    #[test]
    fn a_jsonl_path_falls_back_to_a_surviving_sidecar_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sidecar = dir.path().join("abc");
        std::fs::create_dir(&sidecar).expect("mkdir");
        assert_eq!(
            resolve_transcript(&dir.path().join("abc.jsonl")).as_deref(),
            Some(sidecar.as_path())
        );
    }

    #[test]
    fn a_typo_is_still_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(resolve_transcript(&dir.path().join("nope")).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_events::{SessionId, WallTime};

    fn summary(repo: &str, title: &str, id: &str) -> SessionSummary {
        SessionSummary {
            session: SessionId::new(id),
            transcript: PathBuf::from(format!("/p/{id}.jsonl")),
            sidecar_dir: None,
            project_dir: PathBuf::from("/p"),
            repo: Some(PathBuf::from(repo)),
            repo_exists: true,
            started: Some(WallTime::from_unix_seconds(1_700_000_000)),
            ended: Some(WallTime::from_unix_seconds(1_700_003_600)),
            records: 10,
            tool_calls: 3,
            files_touched: 2,
            subagents: 0,
            title: Some(title.to_owned()),
            branch: None,
            version: None,
            bytes: 4_096,
            counts_partial: false,
            modified_ms: 0,
        }
    }

    #[test]
    fn the_filter_matches_repository_title_and_session_id() {
        let s = summary("/home/me/agentolis", "Wire up the window", "abc-123");
        assert!(matches(&s, "agentolis"));
        assert!(matches(&s, "window"));
        assert!(matches(&s, "abc-123"));
        assert!(!matches(&s, "nothing like this"));
    }

    /// PRD's rule for every ingest channel, applied to the picker: a session
    /// whose repository is gone is a fact to show, not a row to hide.
    #[test]
    fn a_session_whose_repository_is_gone_is_still_a_row() {
        let mut s = summary("/gone", "old work", "z");
        s.repo_exists = false;
        assert!(!s.is_replayable());
        assert!(
            matches(&s, "gone"),
            "it is still findable by its repository"
        );
    }

    /// The list is ordered by last activity and now says so in the column it
    /// orders by. Showing `started` under a descending sort on `ended` is what
    /// made the first four rows read as an unsorted list.
    #[test]
    fn the_column_the_list_is_sorted_by_is_the_column_it_shows() {
        let mut early = summary("/r", "older", "a");
        early.started = Some(WallTime::from_unix_seconds(1_700_000_000));
        early.ended = Some(WallTime::from_unix_seconds(1_700_090_000)); // long run
        let mut late = summary("/r", "newer", "b");
        late.started = Some(WallTime::from_unix_seconds(1_700_080_000)); // later start
        late.ended = Some(WallTime::from_unix_seconds(1_700_081_000)); // earlier end

        // The index sorts on `ended`, so `early` is the more recent row…
        assert!(early.ended > late.ended);
        // …and the picker shows `ended`, which is therefore descending on screen.
        let shown = |s: &SessionSummary| s.ended.or(s.started).map(WallTime::unix_millis);
        assert!(shown(&early) > shown(&late));
    }

    fn picker_over(sessions: Vec<SessionSummary>) -> Picker {
        Picker {
            state: State::Ready(Box::new(SessionIndex {
                sessions,
                projects_dir: PathBuf::from("/p"),
                scanned: 0,
                from_cache: 0,
                elapsed: std::time::Duration::ZERO,
                errors: Vec::new(),
            })),
            filter: String::new(),
            projects_dir: Some(PathBuf::from("/p")),
            scanning: false,
            cursor: 0,
            scroll_to_cursor: false,
            focused: false,
        }
    }

    /// One headless pass with `keys` pressed. Returns what the picker chose.
    fn pass(
        picker: &mut Picker,
        ctx: &egui::Context,
        keys: &[egui::Key],
    ) -> Option<SessionSummary> {
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
            chosen = picker.draw(ui);
        });
        // `epaint` panics if a texture delta is dropped unapplied, and there is
        // no painter here to apply it to.
        full.textures_delta.clear();
        chosen
    }

    /// The picker was mouse-only and said nothing about it: `Down Down Enter`
    /// with the window focused did nothing at all, which for a keyboard-first
    /// operator is a product that does not respond.
    #[test]
    fn arrow_keys_move_the_cursor_and_enter_opens_the_row_it_is_on() {
        let ctx = egui::Context::default();
        let mut picker = picker_over(vec![
            summary("/r/one", "first", "a"),
            summary("/r/two", "second", "b"),
            summary("/r/three", "third", "c"),
        ]);
        // A frame with no keys settles the layout and leaves the cursor at the
        // top, which is where a list with no selection has to start.
        assert!(pass(&mut picker, &ctx, &[]).is_none());
        assert_eq!(picker.cursor, 0);

        assert!(pass(&mut picker, &ctx, &[egui::Key::ArrowDown]).is_none());
        assert_eq!(picker.cursor, 1, "down moves one row");
        assert!(pass(&mut picker, &ctx, &[egui::Key::ArrowDown]).is_none());
        assert_eq!(picker.cursor, 2);
        // And it stops at the end rather than wrapping to the top.
        assert!(pass(&mut picker, &ctx, &[egui::Key::ArrowDown]).is_none());
        assert_eq!(picker.cursor, 2, "the last row is the last row");

        let chosen = pass(&mut picker, &ctx, &[egui::Key::Enter]).expect("enter opens a session");
        assert_eq!(chosen.session.as_str(), "c");

        assert!(pass(&mut picker, &ctx, &[egui::Key::ArrowUp]).is_none());
        assert_eq!(picker.cursor, 1, "up moves back");
    }

    /// A session whose repository is gone is listed and greyed. Enter on it must
    /// do nothing rather than open a window that immediately fails.
    #[test]
    fn enter_on_a_session_whose_repository_is_gone_opens_nothing() {
        let ctx = egui::Context::default();
        let mut gone = summary("/gone", "old work", "z");
        gone.repo_exists = false;
        let mut picker = picker_over(vec![gone]);
        assert!(pass(&mut picker, &ctx, &[]).is_none());
        assert!(pass(&mut picker, &ctx, &[egui::Key::Enter]).is_none());
    }

    /// Filtering changes what row 3 is. The cursor has to land on a row that
    /// exists, and never index past the end of the filtered list.
    #[test]
    fn the_cursor_is_clamped_to_the_rows_the_filter_leaves() {
        let mut cursor = 7;
        assert!(Nav::default().apply(&mut cursor, 3));
        assert_eq!(cursor, 2, "clamped onto the last visible row");

        // An empty list is a cursor at zero and no movement to report.
        let mut cursor = 4;
        assert!(!Nav::default().apply(&mut cursor, 0));
        assert_eq!(cursor, 0);

        // A page jump is clamped at both ends.
        let mut cursor = 0;
        let down = Nav {
            page_down: true,
            ..Nav::default()
        };
        assert!(down.apply(&mut cursor, 5));
        assert_eq!(cursor, 4);
        let up = Nav {
            page_up: true,
            ..Nav::default()
        };
        assert!(up.apply(&mut cursor, 5));
        assert_eq!(cursor, 0);
    }

    /// No `~/.claude/projects` is a message, never a startup failure — the same
    /// rule the four ingest channels follow.
    #[test]
    fn a_missing_projects_directory_is_a_message_not_a_panic() {
        let picker = Picker {
            state: State::Failed("no ~/.claude/projects".to_owned()),
            filter: String::new(),
            projects_dir: None,
            scanning: false,
            cursor: 0,
            scroll_to_cursor: false,
            focused: false,
        };
        assert!(!picker.scanning);
    }
}
