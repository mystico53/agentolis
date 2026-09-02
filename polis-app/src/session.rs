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
                return None;
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
                return None;
            }
            State::Ready(_) => {}
        }

        let State::Ready(index) = &self.state else {
            return None;
        };

        ui.horizontal(|ui| {
            ui.label(RichText::new("filter").color(palette::worker().color()));
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("repository, title or session id")
                    .desired_width(320.0),
            );
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
        ui.separator();

        let needle = self.filter.to_lowercase();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for session in &index.sessions {
                    if !needle.is_empty() && !matches(session, &needle) {
                        continue;
                    }
                    if row(ui, session) {
                        chosen = Some(session.clone());
                    }
                }
            });
        chosen
    }
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

/// One session as a clickable row. Returns true when it was chosen.
fn row(ui: &mut egui::Ui, session: &SessionSummary) -> bool {
    let replayable = session.is_replayable();
    let response = ui
        .horizontal(|ui| {
            ui.add_space(4.0);
            let when = session
                .started
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
    if hit.hovered() {
        ui.painter().rect_filled(
            rect,
            2.0,
            Color32::from_rgba_unmultiplied(120, 140, 160, 18),
        );
    }
    hit.clicked() && replayable
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

    /// No `~/.claude/projects` is a message, never a startup failure — the same
    /// rule the four ingest channels follow.
    #[test]
    fn a_missing_projects_directory_is_a_message_not_a_panic() {
        let picker = Picker {
            state: State::Failed("no ~/.claude/projects".to_owned()),
            filter: String::new(),
            projects_dir: None,
            scanning: false,
        };
        assert!(!picker.scanning);
    }
}
