//! The egui overlay (PRD §12, §13).
//!
//! > **Text lives in a UI overlay, not the GPU layer.** Project world
//! > coordinates to screen and position DOM/`egui` labels. Text rendering is the
//! > classic time sink in custom renderers, and skipping it entirely gets you
//! > crisp glyphs and free styling.
//!
//! Everything with a glyph in it lives here or in [`crate::mapview`]'s label
//! pass. The map layer draws only geometry.
//!
//! # Fuzzy above, exact below
//!
//! PRD §12 splits the window in two, and this module is the *below*:
//!
//! > Soft cloud edges are honest for the ambient layer and useless when acting.
//! > Hovering a building must give a definite list of which threads touched it
//! > and when.
//!
//! So the hover card is a list of thread ids with times, not a summary; the
//! detail panel is operations in order, not a count; and the status bar prints
//! the dropped-event counter as a number even when it is zero, because "no
//! events were dropped" and "the counter is not wired up" must never look the
//! same (PRD §4.5).

// Each of the long functions here is one panel drawn top to bottom, and every
// line of it is a row the operator reads in that order. Cutting a panel into
// fragments called once would put the order of the panel in one function and its
// content in another.
#![allow(clippy::too_many_lines)]

use std::time::Duration;

use eframe::egui::{self, Color32, RichText};
use polis_events::LogicalPath;
use polis_render::camera::ZoomTier;
use polis_world::attention::AttentionKind;
use polis_world::replay::{Interest, ReplayProgress};
use polis_world::snapshot::WorldSnapshot;
use polis_world::{Thread, ThreadStatus};

use crate::format;
use crate::mapview::{thread_label, ViewState};
use crate::palette;

/// Which of the two co-equal renderings the central area is showing (PRD §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    /// The map.
    #[default]
    Map,
    /// The linked filesystem tree.
    Tree,
}

impl View {
    /// The other one. `T` swaps.
    #[must_use]
    pub fn swapped(self) -> Self {
        match self {
            Self::Map => Self::Tree,
            Self::Tree => Self::Map,
        }
    }

    /// What the status bar calls it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Map => "map",
            Self::Tree => "tree",
        }
    }
}

/// The overlay's persistent state: panel visibility and the help sheet.
#[derive(Debug)]
pub struct Overlay {
    /// Which rendering the central area shows.
    pub view: View,
    /// Whether the right-hand rail is open.
    pub rail: bool,
    /// Whether the key sheet is up.
    pub help: bool,
}

impl Default for Overlay {
    fn default() -> Self {
        Self {
            view: View::default(),
            rail: true,
            help: false,
        }
    }
}

/// Everything the status bar reports that is not in the snapshot.
#[derive(Debug, Clone, Copy)]
pub struct Vitals {
    /// `map`, `replay` or `picker`.
    pub mode: &'static str,
    /// Which rendering is showing.
    pub view: View,
    /// Which semantic-zoom representation the map drew.
    pub tier: ZoomTier,
    /// Zoom as a multiple of fit.
    pub zoom: f32,
    /// A typical building's width on screen — the number the tier is decided on.
    pub building_px: f32,
    /// Median frame time over the last few seconds.
    pub frame_p50: f64,
    /// 99th percentile frame time.
    pub frame_p99: f64,
    /// Labels drawn and labels decluttered away.
    pub labels: (usize, usize),
    /// Buildings drawn as vectors.
    pub buildings: usize,
    /// Clouds shown and kernels splatted.
    pub clouds: (usize, usize),
    /// Cold start, process to first frame.
    pub cold_start_ms: f64,
}

/// The status bar (PRD §4.5, §17).
///
/// > a schema-drift warning in the status bar rather than a crash
///
/// It also shows when subagent attribution is degraded, because a Polis
/// silently attributing every worker's edits to its main agent looks exactly
/// like a Polis that is working.
pub fn status_bar(ui: &mut egui::Ui, snapshot: &WorldSnapshot, vitals: Vitals) {
    let health = &snapshot.health;
    let dropped: u64 = health.dropped.values().sum();
    ui.horizontal(|ui| {
        ui.label(chip(vitals.mode, palette::selection().color()));
        ui.label(chip(vitals.view.label(), palette::worker().color()));
        ui.label(chip(
            match vitals.tier {
                ZoomTier::City => "city",
                ZoomTier::District => "district",
                ZoomTier::Building => "building",
            },
            palette::district_label().color(),
        ));
        ui.label(dim(format!(
            "{:.1}× · {:.0} px/building",
            vitals.zoom, vitals.building_px
        )));

        ui.separator();

        // PRD §4.5 requires the dropped counter surfaced. Always a number.
        let dropped_colour = if dropped == 0 {
            palette::status(ThreadStatus::Idle).color()
        } else {
            palette::contention().color()
        };
        ui.label(
            RichText::new(format!("dropped {dropped}"))
                .monospace()
                .color(dropped_colour),
        )
        .on_hover_text(if health.dropped.is_empty() {
            "No channel has reported a drop.".to_owned()
        } else {
            health
                .dropped
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect::<Vec<_>>()
                .join("\n")
        });

        // PRD §17: schema drift is a warning, never a crash.
        let drift_colour = if health.drift == 0 {
            palette::status(ThreadStatus::Idle).color()
        } else {
            palette::needs_decision().color()
        };
        ui.label(
            RichText::new(format!("drift {}", health.drift))
                .monospace()
                .color(drift_colour),
        )
        .on_hover_text(
            "Records of a type this build does not model. Both the OTel and JSONL \
             schemas are beta or undocumented; the parser degrades and counts, it \
             never panics.",
        );

        if !health.degraded.is_empty() {
            ui.label(
                RichText::new(format!("degraded {}", health.degraded.len()))
                    .monospace()
                    .color(palette::needs_decision().color()),
            )
            .on_hover_text(
                health
                    .degraded
                    .iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }

        if health.subagent_attribution_degraded || health.unattributed_workers > 0 {
            ui.label(
                RichText::new(format!("unattributed {}", health.unattributed_workers))
                    .monospace()
                    .color(palette::needs_decision().color()),
            )
            .on_hover_text(
                "Workers with no thread to belong to. Attributing their edits to the \
                 main agent would look exactly like a Polis that is working, so they \
                 are counted here and listed in the rail instead.",
            );
        }

        if health.unmapped_paths > 0 {
            ui.label(dim(format!("off-repo paths {}", health.unmapped_paths)))
                .on_hover_text(
                    "Paths outside every known checkout — a scratch directory, \
                     ~/.claude, another drive. They have no building and are not drawn.",
                );
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let over = vitals.frame_p99 > 16.6;
            ui.label(
                RichText::new(format!(
                    "frame {:.1}/{:.1} ms",
                    vitals.frame_p50, vitals.frame_p99
                ))
                .monospace()
                .color(if over {
                    palette::needs_decision().color()
                } else {
                    palette::status(ThreadStatus::Idle).color()
                }),
            )
            .on_hover_text("median / p99 over the last 240 frames. PRD §13.1 budget: 16.6 ms.");
            ui.label(dim(format!("cold start {:.0} ms", vitals.cold_start_ms)));
            ui.label(dim(format!(
                "labels {}+{} dropped · {} buildings · {} clouds ({} kernels)",
                vitals.labels.0,
                vitals.labels.1,
                vitals.buildings,
                vitals.clouds.0,
                vitals.clouds.1
            )));
        });
    });
}

/// The status rail: one row per thread (PRD §6.2, §11).
///
/// A thread whose territory has not converged has **no cloud on the map** and
/// lives here instead, which is the whole reason the rail exists:
///
/// > Until then the thread renders with no cloud — an unplaced marker in the
/// > status rail.
pub fn status_rail(ui: &mut egui::Ui, snapshot: &WorldSnapshot, state: &mut ViewState) {
    ui.label(heading("THREADS"));
    if snapshot.threads.is_empty() {
        ui.label(dim("no threads yet"));
    }
    for thread in &snapshot.threads {
        let ink = palette::status(thread.status);
        let following = state.follow.as_ref() == Some(&thread.id);
        let response = ui
            .horizontal(|ui| {
                ui.label(
                    RichText::new(status_word(thread.status))
                        .monospace()
                        .color(ink.color()),
                );
                ui.label(
                    RichText::new(thread_label(thread))
                        .color(palette::selection().color())
                        .strong(),
                );
                if following {
                    ui.label(
                        RichText::new("following")
                            .small()
                            .color(palette::hover().color()),
                    );
                }
            })
            .response;
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            if let Some(claim) = &thread.territory.claim {
                ui.label(dim(format!("in {}", claim.as_str())));
            } else {
                {
                    // PRD §6.2 asks for the reason, not just the absence.
                    let c = thread.territory.convergence();
                    ui.label(
                        RichText::new("unplaced")
                            .small()
                            .color(palette::needs_decision().color()),
                    )
                    .on_hover_text(format!(
                        "No territory yet: {} observations, ancestor depth {}, mass ratio \
                         {:.2}. PRD §6.2 emits at depth ≥ 2 and mass > 0.70.",
                        c.observations, c.depth, c.mass_ratio
                    ));
                }
            }
            ui.label(dim(format!(
                "{} workers · {} calls · {} fail · +{} -{}",
                thread.workers.len(),
                thread.tool_calls,
                thread.failures,
                thread.lines_added,
                thread.lines_removed
            )));
        });
        if let Some((path, count)) = thread.most_revisited() {
            if count >= 3 {
                // PRD §12: thrashing — "the same building revisited six times".
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(format!("revisited {count}× {}", short_path(path)))
                            .small()
                            .color(palette::needs_decision().color()),
                    );
                });
            }
        }
        if response.clicked() {
            state.follow = if following {
                None
            } else {
                Some(thread.id.clone())
            };
        }
        ui.separator();
    }

    if !snapshot.unattributed.is_empty() {
        ui.label(heading("UNATTRIBUTED WORKERS"));
        for worker in &snapshot.unattributed {
            ui.label(
                RichText::new(format!("{} · {}", worker.id.as_str(), worker.reason))
                    .small()
                    .monospace()
                    .color(palette::needs_decision().color()),
            );
        }
    }

    if !snapshot.attention.is_empty() {
        ui.label(heading("ATTENTION"));
        for mark in &snapshot.attention {
            let (text, ink) = match &mark.kind {
                AttentionKind::NeedsDecision { thread, source, .. } => (
                    format!(
                        "needs decision · {} · {}",
                        source.label(),
                        short_id(thread.as_str())
                    ),
                    palette::needs_decision(),
                ),
                AttentionKind::Done { thread, verified } => (
                    format!(
                        "done, {} · {}",
                        if *verified { "verified" } else { "UNVERIFIED" },
                        short_id(thread.as_str())
                    ),
                    if *verified {
                        palette::done_verified()
                    } else {
                        palette::done_unverified()
                    },
                ),
                AttentionKind::Contention(c) => (
                    format!(
                        "contention · {} · {}",
                        c.severity.label(),
                        short_path(c.path())
                    ),
                    palette::contention(),
                ),
            };
            ui.label(RichText::new(text).small().color(ink.color()));
        }
    }
}

/// The file detail panel (PRD §12, building tier).
///
/// > **Building** — file detail panel: recent operations, which threads touched
/// > it, diff size, verification status.
pub fn building_panel(ui: &mut egui::Ui, snapshot: &WorldSnapshot, path: &LogicalPath) {
    ui.label(heading("FILE"));
    ui.label(
        RichText::new(path.as_str())
            .monospace()
            .color(palette::selection().color()),
    );

    let Some(file) = snapshot.file(path) else {
        ui.label(dim("no agent has touched this file in this recording"));
        if snapshot.layout.buildings.contains_key(path) {
            ui.label(dim("it has a building; the map is drawing it at rest"));
        } else {
            ui.label(
                RichText::new("no building — this file is not on the map")
                    .small()
                    .color(palette::needs_decision().color()),
            );
        }
        return;
    };

    ui.horizontal(|ui| {
        ui.label(dim("diff"));
        ui.label(
            RichText::new(format!("+{} −{}", file.lines_added, file.lines_removed))
                .monospace()
                .color(palette::outcome(polis_events::Outcome::Done).color()),
        );
        ui.label(dim(format!(
            "({} lines, {:?})",
            file.diff_lines, file.diff_precision
        )))
        .on_hover_text(
            "Precision is never laundered upward: an approximate count from a \
             filesystem event stays approximate until an authoritative channel \
             replaces it.",
        );
    });
    ui.horizontal(|ui| {
        ui.label(dim("reads / writes"));
        ui.label(
            RichText::new(format!("{} / {}", file.reads, file.writes))
                .monospace()
                .color(palette::trail().color()),
        );
        if file.deleted {
            ui.label(RichText::new("deleted").color(palette::contention().color()));
        }
    });
    ui.horizontal(|ui| {
        ui.label(dim("verified"));
        if file.is_verified() {
            ui.label(
                RichText::new("yes — tests ran after the last change")
                    .color(palette::done_verified().color()),
            );
        } else {
            ui.label(
                RichText::new("no — this is really \"needs review\"")
                    .color(palette::done_unverified().color()),
            );
        }
    });

    // "Fuzzy above, exact below": a definite list, with times.
    ui.label(heading("TOUCHED BY"));
    if file.touched_by.is_empty() {
        ui.label(dim("nobody"));
    }
    for id in &file.touched_by {
        let thread = snapshot.thread(id);
        let label = thread.map_or_else(|| short_id(id.as_str()), thread_label);
        let when = thread.and_then(|t| t.visits.get(path)).map_or_else(
            || "—".to_owned(),
            |v| {
                format!(
                    "{} visits, last {} ago",
                    v.count,
                    format::duration(snapshot.at.saturating_duration_since(v.last))
                )
            },
        );
        ui.label(
            RichText::new(format!("{label} · {when}"))
                .small()
                .color(palette::worker().color()),
        );
    }

    ui.label(heading("RECENT OPERATIONS"));
    let mut shown = 0;
    for thread in &snapshot.threads {
        for op in thread.ops.iter().rev() {
            if op.path.as_ref() != Some(path) {
                continue;
            }
            ui.label(
                RichText::new(format!(
                    "{:>8} ago  {:?}  {:?}  {}",
                    format::duration(snapshot.at.saturating_duration_since(op.at)),
                    op.tool,
                    op.outcome,
                    op.worker
                        .as_ref()
                        .map_or_else(|| "-".to_owned(), |w| short_id(w.as_str()))
                ))
                .small()
                .monospace()
                .color(palette::outcome(op.outcome).color()),
            );
            shown += 1;
            if shown >= 12 {
                return;
            }
        }
    }
    if shown == 0 {
        ui.label(dim("none in the window the world keeps"));
    }
}

/// The hover card: PRD §12's *"a definite list of which threads touched it and
/// when"*, at the pointer.
pub fn hover_card(
    ctx: &egui::Context,
    at: egui::Pos2,
    snapshot: &WorldSnapshot,
    path: &LogicalPath,
) {
    egui::Area::new(egui::Id::new("polis-hover"))
        .order(egui::Order::Tooltip)
        .fixed_pos(at + egui::Vec2::new(16.0, 16.0))
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style())
                .fill(Color32::from_rgb(14, 16, 20))
                .show(ui, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(
                        RichText::new(path.as_str())
                            .monospace()
                            .color(palette::selection().color()),
                    );
                    match snapshot.file(path) {
                        None => {
                            ui.label(dim("untouched in this recording"));
                        }
                        Some(file) => {
                            ui.label(
                                RichText::new(format!(
                                    "+{} −{} · {} reads · {} writes{}",
                                    file.lines_added,
                                    file.lines_removed,
                                    file.reads,
                                    file.writes,
                                    if file.is_verified() {
                                        " · verified"
                                    } else {
                                        ""
                                    }
                                ))
                                .small()
                                .monospace()
                                .color(palette::trail().color()),
                            );
                            for id in &file.touched_by {
                                let thread = snapshot.thread(id);
                                let name =
                                    thread.map_or_else(|| short_id(id.as_str()), thread_label);
                                let when = thread.and_then(|t| t.visits.get(path)).map_or_else(
                                    || "—".to_owned(),
                                    |v| {
                                        format!(
                                            "{}× · last {} ago",
                                            v.count,
                                            format::duration(
                                                snapshot.at.saturating_duration_since(v.last)
                                            )
                                        )
                                    },
                                );
                                ui.label(
                                    RichText::new(format!("{name} · {when}"))
                                        .small()
                                        .color(palette::worker().color()),
                                );
                            }
                        }
                    }
                    ui.label(
                        RichText::new("click to open in your editor")
                            .small()
                            .color(palette::status(ThreadStatus::Idle).color()),
                    );
                });
        });
}

/// What the transport bar was asked to do this frame.
///
/// Flags rather than an enum because a frame can carry several at once — a
/// click on `⏭` while `]` is held is a step *and* a speed change — and the
/// caller applies them in a fixed order.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct TransportAction {
    /// Toggle play/pause.
    pub toggle: bool,
    /// Apply exactly one event.
    pub step: bool,
    /// Go back one event.
    pub step_back: bool,
    /// Seek to this fraction of the playback timeline.
    pub seek: Option<f32>,
    /// Set the speed.
    pub speed: Option<f32>,
    /// One step up the speed ladder.
    pub faster: bool,
    /// One step down.
    pub slower: bool,
    /// Jump to the next interesting moment.
    pub next_interesting: bool,
    /// Return to the session picker.
    pub back_to_picker: bool,
}

/// The replay transport (PRD §12, §15 M2).
///
/// Everything the operator needs to steer a recording: play, pause, speed, a
/// scrubber over the session timeline, single steps, and the jump that makes a
/// twenty-six-hour session watchable — *next interesting moment*.
pub fn transport(
    ui: &mut egui::Ui,
    progress: &ReplayProgress,
    label: &str,
    next: Option<Interest>,
    compressed: Option<Duration>,
) -> TransportAction {
    let mut action = TransportAction::default();
    ui.horizontal(|ui| {
        if ui
            .button(if progress.playing { "⏸" } else { "▶" })
            .on_hover_text("space")
            .clicked()
        {
            action.toggle = true;
        }
        if ui.button("⏮").on_hover_text(", — one event back").clicked() {
            action.step_back = true;
        }
        if ui.button("⏭").on_hover_text(". — one event on").clicked() {
            action.step = true;
        }

        ui.label(dim("speed"));
        for speed in [0.5f32, 1.0, 4.0, 16.0, 64.0] {
            let on = (progress.speed - speed).abs() < 1e-3;
            if ui
                .selectable_label(on, format!("{}×", trim(speed)))
                .clicked()
            {
                action.speed = Some(speed);
            }
        }

        let hint = next.map_or_else(
            || "no further marked moment".to_owned(),
            |k| format!("next: {}", k.label()),
        );
        if ui
            .add_enabled(next.is_some(), egui::Button::new("⏩ next moment"))
            .on_hover_text(format!("{hint}  (n)"))
            .clicked()
        {
            action.next_interesting = true;
        }

        if ui.button("sessions…").on_hover_text("p").clicked() {
            action.back_to_picker = true;
        }
    });

    ui.horizontal(|ui| {
        let mut fraction = progress.fraction();
        // A scrubber over a 26-hour session at `Slider`'s 100-point default is
        // 15 minutes per pixel. It gets the width that is left.
        ui.spacing_mut().slider_width = (ui.available_width() - 620.0).max(160.0);
        let scrubber = ui.add(
            egui::Slider::new(&mut fraction, 0.0..=1.0)
                .show_value(false)
                .trailing_fill(true),
        );
        if scrubber.changed() {
            action.seek = Some(fraction);
        }
        ui.label(
            RichText::new(format!(
                "{} / {}",
                format::duration(progress.session_elapsed()),
                format::duration(Duration::from_millis(progress.session_duration_ms))
            ))
            .monospace()
            .color(palette::selection().color()),
        )
        .on_hover_text("Position in the real session, not in the compressed playback.");
        ui.label(dim(format!(
            "watch {} / {}",
            format::duration(progress.elapsed()),
            format::duration(progress.total())
        )));
        ui.label(
            RichText::new(format!(
                "{} / {} events",
                progress.events_applied, progress.events_total
            ))
            .monospace()
            .color(palette::trail().color()),
        );
        if let Some(wall) = progress.wall {
            ui.label(dim(format::wall_time(wall)));
        }
    });

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(label)
                .monospace()
                .color(palette::district_label().color()),
        );
        if let Some(removed) = compressed {
            if removed > Duration::from_secs(1) {
                ui.label(dim(format!(
                    "{} of dead air removed",
                    format::duration(removed)
                )))
                .on_hover_text(
                    "Idle gaps are capped so a session that ran overnight is watchable. \
                         The world still ages by real session time across each gap.",
                );
            }
        }
    });
    action
}

/// The key sheet.
pub fn help(ui: &mut egui::Ui) {
    ui.label(heading("KEYS"));
    for (key, what) in [
        ("drag / arrows", "pan"),
        ("scroll / + -", "zoom"),
        ("t", "swap map ⇄ filesystem tree"),
        ("click a building", "select it and open it in your editor"),
        ("hover a building", "who touched it, and when"),
        ("f", "follow the selected thread (cut, never pan)"),
        ("r", "reset the camera"),
        ("s", "streets layer (PRD §9)"),
        ("i", "rail"),
        ("space", "play / pause"),
        (", .", "one event back / on"),
        ("[ ]", "slower / faster"),
        ("n", "next interesting moment"),
        ("home / end", "start / end of the recording"),
        ("p", "back to the session picker"),
        ("h or ?", "this sheet"),
        ("esc", "clear the selection"),
    ] {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{key:>16}"))
                    .monospace()
                    .color(palette::hover().color()),
            );
            ui.label(dim(what));
        });
    }
    ui.separator();
    ui.label(heading("NOTATION"));
    ui.label(dim(
        "shape is the operation, colour is how it went (PRD §10.1, §10.2)",
    ));
    crate::mapview::legend(ui);
}

fn status_word(status: ThreadStatus) -> &'static str {
    match status {
        ThreadStatus::Waiting => "WAITING",
        ThreadStatus::Working => "working",
        ThreadStatus::Idle => "idle   ",
        ThreadStatus::Done => "done   ",
    }
}

fn heading(text: &str) -> RichText {
    RichText::new(text)
        .monospace()
        .small()
        .color(palette::district_label().color())
}

fn chip(text: &str, colour: Color32) -> RichText {
    RichText::new(text).monospace().strong().color(colour)
}

fn dim(text: impl Into<String>) -> RichText {
    RichText::new(text.into())
        .small()
        .color(palette::status(ThreadStatus::Idle).color())
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn short_path(path: &LogicalPath) -> String {
    path.file_name()
        .map_or_else(|| path.as_str().to_owned(), str::to_owned)
}

fn trim(speed: f32) -> String {
    if (speed - speed.round()).abs() < 1e-3 {
        // Speeds come from `SPEED_RANGE`, which is 0.25..=64: the cast cannot
        // truncate anything the transport can produce.
        #[allow(clippy::cast_possible_truncation)]
        return format!("{}", speed.round() as i32);
    }
    format!("{speed}")
}

/// A thread's one-line summary, shared by the rail and the transport bar.
pub fn describe_thread(thread: &Thread) -> String {
    format!(
        "{} · {} · {} workers",
        thread_label(thread),
        status_word(thread.status).trim(),
        thread.workers.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_keystroke_swaps_the_two_co_equal_views() {
        assert_eq!(View::Map.swapped(), View::Tree);
        assert_eq!(View::Tree.swapped(), View::Map);
        assert_eq!(View::Map.swapped().swapped(), View::Map);
    }

    #[test]
    fn status_words_are_padded_to_one_column_width() {
        let widths: Vec<usize> = [
            ThreadStatus::Waiting,
            ThreadStatus::Working,
            ThreadStatus::Idle,
            ThreadStatus::Done,
        ]
        .into_iter()
        .map(|s| status_word(s).len())
        .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "the rail is a column: {widths:?}"
        );
    }

    #[test]
    fn a_speed_reads_as_a_multiplier() {
        assert_eq!(trim(1.0), "1");
        assert_eq!(trim(64.0), "64");
        assert_eq!(trim(0.5), "0.5");
    }
}
