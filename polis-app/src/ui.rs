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
use polis_events::{LogicalPath, ThreadId};
use polis_render::camera::ZoomTier;
use polis_world::attention::AttentionKind;
use polis_world::replay::{Interest, ReplayProgress};
use polis_world::snapshot::WorldSnapshot;
use polis_world::{Thread, ThreadStatus};

use crate::drill::{self, Jump};
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
    /// Whether the first-run map explainer is up.
    ///
    /// True until this machine has dismissed it once ([`crate::explain`]).
    /// Nothing else in the window is modal, and this is only modal for one
    /// keystroke: it exists because three of the four ways into the map never
    /// show the terminal screen that explains it.
    pub explain: bool,
    /// Which attention row the `a` key last jumped to.
    ///
    /// > **Primary decision it accelerates:** *unblock* — get to the thread that
    /// > is waiting on a human. (PRD §1)
    ///
    /// `a` walks the list rather than always landing on the first row, so an
    /// operator with three things waiting can visit all three without touching
    /// the pointer. The row it landed on is drawn marked, because a jump that
    /// changes the camera and says nothing about *why* is indistinguishable
    /// from a misclick.
    pub attention_cursor: Option<usize>,
}

impl Default for Overlay {
    fn default() -> Self {
        Self {
            view: View::default(),
            rail: true,
            help: false,
            explain: !crate::explain::dismissed(),
            attention_cursor: None,
        }
    }
}

impl Overlay {
    /// Advances the attention cursor and returns the row to jump to.
    ///
    /// The list is PRD §11.1's order, so the first press lands on the worst
    /// state on the map — contention if there is any, else the oldest thread
    /// waiting on a human. Wrapping rather than stopping, because the operator
    /// is cycling a work queue and the end of it is the start of it.
    pub fn next_attention(&mut self, snapshot: &WorldSnapshot) -> Option<Jump> {
        let rows = drill::ranked(snapshot);
        if rows.is_empty() {
            self.attention_cursor = None;
            return None;
        }
        let next = match self.attention_cursor {
            Some(i) if i + 1 < rows.len() => i + 1,
            // Off the end, or nothing yet: the worst state on the map.
            _ => 0,
        };
        self.attention_cursor = Some(next);
        let row = &rows[next];
        Some(Jump {
            path: row.at.clone(),
            thread: row.mark.thread().clone(),
        })
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
///
/// Returns true when the operator clicked the attention count — the one chip
/// here that is a control rather than a reading, because the rail can be shut
/// and PRD §1's primary decision must never be more than one click away.
pub fn status_bar(ui: &mut egui::Ui, snapshot: &WorldSnapshot, vitals: Vitals) -> bool {
    let health = &snapshot.health;
    let dropped: u64 = health.dropped.values().sum();
    let mut open_attention = false;
    ui.horizontal(|ui| {
        // First on the bar, before anything about frames or labels: the count
        // of things waiting on a human is the only number here that is about
        // the operator's next action.
        if let Some(first) = snapshot.attention.first() {
            let ink = drill::ink(&first.kind);
            if ui
                .add(
                    egui::Button::new(
                        RichText::new(format!(
                            "! {} · {}",
                            snapshot.attention.len(),
                            drill::word(&first.kind)
                        ))
                        .monospace()
                        .strong()
                        .color(ink.color()),
                    )
                    .frame(false),
                )
                .on_hover_text("The attention list, worst first. Click, or press `a` to jump.")
                .clicked()
            {
                open_attention = true;
            }
        }
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
    open_attention
}

/// The thread's own colour, as a rectangle at the head of its row.
///
/// > i'd like one thread to be one color […] matching that color also in the
/// > rail with a rectangle and a matching hover.
///
/// Deliberately a **filled rectangle** and not a tinted label: a swatch is a
/// patch of flat colour with nothing else in it, which is the only shape whose
/// hue can be matched by eye against a cloud or an agent mark out on the map.
/// Tinting the thread's name instead would put the hue behind antialiased
/// glyph edges, where it composites with the panel and stops being the same
/// colour it is on the map.
///
/// The colour is `palette::thread`, which is the agent band's own brightness —
/// so this rectangle and that thread's agent body are the identical triple, and
/// the match the operator is asked to make is exact rather than approximate.
fn swatch(ui: &mut egui::Ui, tint: u8) {
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(9.0, 13.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 1.5, palette::thread(tint).color());
}

/// How many places a lobed thread names before it says "+N".
///
/// Two. The row already carries a status word, a title, a worker count, a call
/// count, a failure count and a diff; a third and fourth district name past the
/// two heaviest stops changing a decision and starts wrapping the row (PRD §17).
const PLACES_NAMED: usize = 2;

/// Where a thread is working, as one line of the rail (PRD §6.2, §6.4).
///
/// The rail's job here is to answer *where*, and for most of this product's life
/// it could only answer it for a thread with a single converged ancestor. Every
/// other thread — including an orchestrator with seventeen workers and 648 tool
/// calls, spread over `src/components`, `src/hooks` and `tests` — read
/// **`unplaced`**, in the same frame in which its cloud was on the map. That is
/// the rail contradicting the map about the same thread, and it is what the
/// operator was looking at when they said they could see no distinction between
/// agents.
///
/// So the three states are drawn as three states:
///
/// * a claim — `in src/services`;
/// * lobes — `in src/components, src/hooks +1`, heaviest first, with the full
///   list and each lobe's share of the thread's weight on hover, because
///   §6.4's lobes are *weighted* and a minor lobe should read as minor;
/// * nowhere — `unplaced`, which is a real state and keeps §6.2's honest
///   reason on hover: the observation count, the ancestor's depth and the mass
///   ratio, so "why has this no cloud" has an answer and not a shrug.
fn place_of(ui: &mut egui::Ui, thread: &polis_world::Thread) {
    use polis_world::territory::Placement;

    match thread.territory.placement() {
        Placement::Claim(claim) => {
            ui.label(dim(format!("in {}", claim.as_str())))
                .on_hover_text(
                    "PRD §6.2: the observations agree on one ancestor, so the thread has one \
                     district and one cloud.",
                );
        }
        Placement::Lobes(lobes) => {
            let named = lobes
                .iter()
                .take(PLACES_NAMED)
                .map(|l| l.path.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let rest = lobes.len().saturating_sub(PLACES_NAMED);
            let line = if rest == 0 {
                format!("in {named}")
            } else {
                format!("in {named} +{rest}")
            };
            let detail = lobes
                .iter()
                .map(|l| format!("  {} — {:.0}% of its work", l.path.as_str(), l.mass * 100.0))
                .collect::<Vec<_>>()
                .join("\n");
            ui.label(dim(line)).on_hover_text(format!(
                "Working in {} places at once, so no single ancestor above the repository root \
                 describes it and PRD §6.2's claim is empty. PRD §6.4 draws it as lobes joined \
                 by a thin band — one entity, several centres.\n\n{detail}",
                lobes.len()
            ));
        }
        Placement::Nowhere => {
            // PRD §6.2 asks for the reason, not just the absence.
            let c = thread.territory.convergence();
            ui.label(
                RichText::new("unplaced")
                    .small()
                    .color(palette::needs_decision().color()),
            )
            .on_hover_text(format!(
                "No territory yet: {} observations, ancestor depth {}, mass ratio {:.2}, and no \
                 cluster heavy enough to be a lobe. PRD §6.2 emits at depth ≥ 2 and mass > 0.70; \
                 PRD §6.4's lobes need {:.0}% of the weight in one place.",
                c.observations,
                c.depth,
                c.mass_ratio,
                polis_world::territory::MIN_LOBE_MASS * 100.0,
            ));
        }
    }
}

/// The status rail: one row per thread (PRD §6.2, §11).
///
/// A thread whose territory has not converged has **no cloud on the map** and
/// lives here instead, which is the whole reason the rail exists:
///
/// > Until then the thread renders with no cloud — an unplaced marker in the
/// > status rail.
///
/// Returns the thread the operator closed with `✕`, if any.
///
/// # Why there is a close button on a map that decays by itself
///
/// Every rule that takes a thread off this rail is a rule about *silence*, and
/// silence is the only evidence a transcript can give: there is no session-end
/// record, so a session the operator closed an hour ago and one sitting at a
/// prompt look identical (`polis_world::World::retire_threads`). The operator is
/// the one party who knows which, and this is the only way to say so. It is not
/// a filter and it remembers nothing — `polis_world::World::dismiss_thread`
/// forgets the thread outright, so the next event for that session brings the
/// row back exactly as a first sighting would.
pub fn status_rail(
    ui: &mut egui::Ui,
    snapshot: &WorldSnapshot,
    state: &mut ViewState,
) -> Option<ThreadId> {
    ui.label(heading("THREADS"));
    if snapshot.threads.is_empty() {
        ui.label(dim("no threads yet"));
    }
    let mut dismissed = None;
    for thread in &snapshot.threads {
        // A session that has made no tool call has told Polis nothing. It gets a
        // transcript the moment it starts — a title record, a mode record — so a
        // freshly-opened terminal, or one the operator opened and never used,
        // arrives here as a row reading "thread 15dd6e8a · unplaced · 0 workers
        // · 0 calls · 0 fail". The operator's report was exact: *"i currently
        // have only one terminal open in that directory. not sure why i see
        // those"*, and *"the ones saying idle are completely unclear what they
        // are doing"* — which is the honest reading, because they are not doing
        // anything.
        //
        // PRD §17: *"does it change a decision? If not, cut it."* A thread with
        // no activity cannot change one, and it costs a row that a working
        // thread needs. It is still counted in the status strip, so the map
        // never silently forgets a session it can see.
        if thread.tool_calls == 0 && thread.workers.is_empty() {
            continue;
        }
        let ink = palette::status(thread.status);
        let tint = palette::thread_slot(&thread.id);
        let following = state.follow.as_ref() == Some(&thread.id);
        let emphasised = state.emphasises(&thread.id);
        let head = ui.horizontal(|ui| {
            swatch(ui, tint);
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
            // Right-aligned, and the only control on the row: a destructive
            // action that sat next to the label would be hit by an operator
            // reaching for "follow this one".
            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    let x = ui
                        .add(
                            egui::Button::new(
                                RichText::new("✕")
                                    .monospace()
                                    .color(palette::status(ThreadStatus::Idle).color()),
                            )
                            .frame(false),
                        )
                        .on_hover_text(
                            "Close this thread. Nothing is remembered, so its next event                              brings it back.",
                        );
                    (x.clicked(), x.rect)
                },
            )
            .inner
        });
        let (closed, close_rect) = head.inner;
        if closed {
            dismissed = Some(thread.id.clone());
        }
        let top = head.response.rect.top();
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            place_of(ui, thread);
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
        // The **whole block** is the row, not just its header line: the place
        // an operator's pointer lands when they mean "that thread" is as often
        // on `in src/services · 32 workers` as on the title, and a 26-pixel
        // target inside a 120-pixel row is a hit area that reads as broken. It
        // is still stopped short of the `✕`, which is a different verb and a
        // destructive one — an operator reaching for "follow this one" must not
        // find it.
        //
        // `interact` rather than the `horizontal`'s own response, because a
        // `Ui` scope senses hover only and its `clicked()` is never true:
        // click-to-follow here did nothing at all.
        let target = egui::Rect::from_min_max(
            egui::Pos2::new(ui.max_rect().left(), top),
            egui::Pos2::new(close_rect.left() - 4.0, ui.min_rect().bottom()),
        );
        let hit = ui.interact(
            target,
            ui.id().with(("thread", thread.id.as_str())),
            egui::Sense::click(),
        );
        if hit.hovered() {
            // PRD §12's shared highlight, written from the rail; the map lights
            // that thread's marks up when it reads it.
            state.hover_thread(&thread.id);
        }
        // Painted from the shared state as well as from this frame's pointer,
        // so a row lights up identically whether the pointer is on the row or
        // out on that thread's cloud — which is the whole of "and vice versa".
        // Two weights, because a selection outlives the pointer and a hover
        // does not.
        if emphasised || hit.hovered() {
            let selected = state.selected_thread.as_ref() == Some(&thread.id);
            let wash = if selected { 0.14 } else { 0.09 };
            ui.painter()
                .rect_filled(target, 2.0, palette::thread(tint).alpha(wash));
            // A bar down the left edge in the thread's own colour. The wash
            // alone is a colour difference and nothing else, which a
            // colour-blind operator and a downscaled screenshot both lose; the
            // bar is an edge, and an edge survives both (PRD §11.4).
            ui.painter().rect_filled(
                egui::Rect::from_min_max(
                    target.min,
                    egui::Pos2::new(target.min.x + 3.0, target.max.y),
                ),
                0.0,
                palette::thread(tint).alpha(if selected { 1.0 } else { 0.75 }),
            );
        }
        if hit.clicked() {
            // One click does both verbs, because on this row they are one
            // intention: "this thread". Follow toggles — it owns the camera and
            // an operator must be able to give it back — while the selection
            // moves to whatever was clicked last, so the map's highlight always
            // matches the row the operator just used.
            state.follow = if following {
                None
            } else {
                Some(thread.id.clone())
            };
            state.selected_thread = Some(thread.id.clone());
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
    dismissed
}

/// The attention list: every live state, worst first, one click from the thing
/// it is about (PRD §1, §11.1, §12).
///
/// > **Primary decision it accelerates:** *unblock* — get to the thread that is
/// > waiting on a human.
///
/// The map answers *where*, at a glance, from across the room. This answers
/// *which one first, and take me there* — and until it existed nothing in the
/// window did, so an operator who had seen an amber pin still had to find it by
/// panning. The order is `snapshot.attention`'s own: `polis-world` publishes in
/// PRD §11.1's `contention > needs-decision > done` and a second opinion about
/// severity here would be a second product.
///
/// Returns the row the operator clicked.
pub fn attention_list(
    ui: &mut egui::Ui,
    snapshot: &WorldSnapshot,
    cursor: Option<usize>,
) -> Option<Jump> {
    let rows = drill::ranked(snapshot);
    ui.horizontal(|ui| {
        ui.label(heading("ATTENTION"));
        if !rows.is_empty() {
            ui.label(
                RichText::new(format!("{}", rows.len()))
                    .monospace()
                    .strong()
                    .color(drill::ink(&rows[0].mark.kind).color()),
            );
        }
        ui.label(dim("(a) jumps to the next"));
    });
    if rows.is_empty() {
        ui.label(dim("nothing is waiting on you"));
        ui.separator();
        return None;
    }

    let now = snapshot.at;
    let mut jump = None;
    // PRD §17: *"does it change a decision? If not, cut it."* Measured on the
    // operator's own live session, this list opened with **33 rows**, all of
    // them "contention · same file · two workers of one agent", which pushed
    // every other state and the whole thread rail off the bottom of the rail.
    // Thirty-three of one kind is one decision, not thirty-three — so each kind
    // gets a few rows and a count of what is behind them, and `a` still walks
    // every one of them.
    //
    // Decided before anything is drawn, because the "+N more" line has to sit
    // directly under the rows it is counting and that is not knowable until the
    // whole list has been walked.
    let (keep, hidden, last_of) = trim_rows(&rows, cursor);
    for (i, row) in rows.iter().enumerate() {
        if !keep[i] {
            continue;
        }
        let rank = drill::rank(&row.mark.kind) as usize;
        let ink = drill::ink(&row.mark.kind);
        // One `vertical` around both lines, so the rectangle below is this row's
        // and not "everything drawn so far". `ui.min_rect()` grows with the
        // panel, and using it here gave every row a hit target reaching back to
        // the top of the rail — so a click anywhere in the list jumped to
        // whichever row happened to be registered last.
        let block = ui
            .vertical(|ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if cursor == Some(i) {
                        ui.label(
                            RichText::new("▸")
                                .monospace()
                                .color(palette::hover().color()),
                        );
                    }
                    // Shape and word first, colour second: PRD §11.4 is explicit
                    // that colour alone is never the sole channel for any state.
                    ui.label(
                        RichText::new(drill::word(&row.mark.kind))
                            .monospace()
                            .strong()
                            .color(ink.color()),
                    );
                    ui.label(
                        RichText::new(detail(&row.mark.kind))
                            .small()
                            .color(ink.alpha(0.85)),
                    );
                    ui.label(
                        RichText::new(format::duration(row.waiting))
                            .small()
                            .monospace()
                            .color(palette::selection().color()),
                    );
                });
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(&row.thread)
                            .small()
                            .color(palette::worker().color()),
                    );
                    match &row.at {
                        Some(path) => {
                            ui.label(
                                RichText::new(drill::label_of(path))
                                    .small()
                                    .monospace()
                                    .color(palette::file_label().color()),
                            );
                        }
                        None => {
                            ui.label(dim("nowhere on the map yet"));
                        }
                    }
                });
            })
            .response
            .rect;

        // The whole two-line block is the target, widened to the panel so the
        // gap after a short row is still the row. The arrival pulse plays here
        // too — the list is on screen when the map is not (PRD §11.4).
        let block = egui::Rect::from_min_max(
            egui::Pos2::new(ui.max_rect().left(), block.top()),
            egui::Pos2::new(ui.max_rect().right(), block.bottom()),
        );
        let hit = ui.interact(block, ui.id().with(("attention", i)), egui::Sense::click());
        let pulse = row.mark.pulse(now);
        if pulse > 0.0 {
            ui.painter()
                .rect_filled(block, 2.0, ink.alpha(pulse * 0.25));
        } else if hit.hovered() {
            ui.painter()
                .rect_filled(block, 2.0, palette::hover().alpha(0.06));
        }
        if hit.clicked() {
            jump = Some(Jump {
                path: row.at.clone(),
                thread: row.mark.thread().clone(),
            });
        }
        if last_of[rank] == Some(i) && hidden[rank] > 0 {
            ui.label(
                RichText::new(format!("+{} more {}", hidden[rank], KIND_NAMES[rank]))
                    .small()
                    .color(palette::status(ThreadStatus::Idle).color()),
            )
            .on_hover_text(
                "Hidden so the states below them stay visible. `a` still walks every \
                 one of them, and the map draws them all.",
            );
        }
        ui.separator();
    }
    jump
}

/// Which rows survive the per-kind cap, how many of each kind did not, and the
/// index of the last surviving row of each kind.
fn trim_rows(
    rows: &[drill::Ranked<'_>],
    cursor: Option<usize>,
) -> (Vec<bool>, [usize; 4], [Option<usize>; 4]) {
    let mut keep = vec![false; rows.len()];
    let mut shown = [0usize; 4];
    let mut hidden = [0usize; 4];
    let mut last_of = [None; 4];
    for (i, row) in rows.iter().enumerate() {
        let rank = drill::rank(&row.mark.kind) as usize;
        // The row the `a` key is standing on is never one of the hidden ones.
        if shown[rank] < ROWS_PER_KIND || cursor == Some(i) {
            shown[rank] += 1;
            keep[i] = true;
            last_of[rank] = Some(i);
        } else {
            hidden[rank] += 1;
        }
    }
    (keep, hidden, last_of)
}

/// How many rows of each attention kind the list shows before it starts
/// counting instead.
pub const ROWS_PER_KIND: usize = 4;

/// PRD §11.1's four kinds, in rank order, for the *"+N more"* line.
const KIND_NAMES: [&str; 4] = ["contention", "waiting on you", "needs review", "done"];

/// The second half of an attention row: what kind of decision, or how bad the
/// collision is.
fn detail(kind: &AttentionKind) -> String {
    match kind {
        AttentionKind::NeedsDecision { source, .. } => source.label().to_owned(),
        AttentionKind::Done { verified: true, .. } => "tests ran after the change".to_owned(),
        AttentionKind::Done {
            verified: false, ..
        } => "no test ran after the change".to_owned(),
        AttentionKind::Contention(c) => {
            let who = if c.is_within_thread() {
                "two workers of one agent"
            } else {
                "two agents"
            };
            format!("{} · {who}", c.severity.label())
        }
    }
}

/// What the detail panel was asked to do this frame.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PanelAction {
    /// Open the subject in `$EDITOR` (PRD §12) — the button, for when the file
    /// was reached from the tree or the attention list rather than by clicking
    /// its building.
    pub open_editor: bool,
    /// Bind the camera to this thread.
    pub follow: Option<ThreadId>,
}

/// The file detail panel — PRD §12's third semantic-zoom tier.
///
/// > **Building** — file detail panel: recent operations, which threads touched
/// > it, diff size, verification status.
///
/// All four, in that order, from [`drill::facts`] — the same computation the
/// hover card and the tree badges read, so the three cannot disagree about who
/// touched this file. Every row is exact: counts, not adjectives; times, not
/// "recently"; and the list of touchers includes threads that have since ended,
/// because *"a definite list"* means the answer does not shrink when an agent
/// exits.
pub fn building_panel(
    ui: &mut egui::Ui,
    snapshot: &WorldSnapshot,
    path: &LogicalPath,
) -> PanelAction {
    let mut action = PanelAction::default();
    let facts = drill::facts(snapshot, path);

    ui.horizontal_wrapped(|ui| {
        ui.label(heading(if facts.district { "DIRECTORY" } else { "FILE" }));
        if !facts.on_map && !facts.district {
            ui.label(
                RichText::new("off-map")
                    .small()
                    .monospace()
                    .color(palette::needs_decision().color()),
            )
            .on_hover_text(
                "No building in the current layout — created since the walk, in \
                 another worktree, or under a path the mapper could not resolve. \
                 The map cannot draw it; this panel and the tree can.",
            );
        }
    });
    ui.label(
        RichText::new(drill::label_of(path))
            .monospace()
            .color(palette::selection().color()),
    );
    ui.horizontal(|ui| {
        if ui
            .button("open in editor")
            .on_hover_text("The same thing a click on its building does, and nothing more.")
            .clicked()
        {
            action.open_editor = true;
        }
    });

    // Contention and a decision waiting at this very building outrank
    // everything else on the panel, so they are above the fold (PRD §11.1).
    for mark in &facts.marks {
        let ink = drill::ink(&mark.kind);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(
                RichText::new(drill::word(&mark.kind))
                    .monospace()
                    .strong()
                    .color(ink.color()),
            );
            ui.label(
                RichText::new(detail(&mark.kind))
                    .small()
                    .color(ink.alpha(0.9)),
            );
        });
        if let AttentionKind::Contention(c) = &mark.kind {
            let (a, b) = c.actors();
            ui.horizontal_wrapped(|ui| {
                ui.add_space(10.0);
                ui.label(
                    RichText::new(format!("{} <-> {}", actor_label(&a), actor_label(&b)))
                        .small()
                        .monospace()
                        .color(palette::contention().color()),
                );
                ui.label(dim(format!("{:?}", c.precision))).on_hover_text(
                    "FileLevel means at least one claim carried no line range — always \
                     the case for a subagent edit — so the tier stops at \"same file\" \
                     rather than guessing at overlap.",
                );
            });
        }
    }

    let (added, removed) = facts.diff();
    ui.horizontal(|ui| {
        ui.label(dim("diff"));
        ui.label(
            RichText::new(format!("+{added} −{removed}"))
                .monospace()
                .color(palette::outcome(polis_events::Outcome::Done).color()),
        );
        if let Some(file) = facts.state {
            ui.label(dim(format!(
                "({} lines, {:?})",
                file.diff_lines, file.diff_precision
            )))
            .on_hover_text(
                "Precision is never laundered upward: an approximate count from a \
                 filesystem event stays approximate until an authoritative channel \
                 replaces it.",
            );
            if let Some(total) = file.total_lines {
                ui.label(dim(format!("of {total}")));
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label(dim("reads / writes"));
        ui.label(
            RichText::new(format!(
                "{} / {}",
                facts.state.map_or(0, |f| f.reads),
                facts.state.map_or(0, |f| f.writes)
            ))
            .monospace()
            .color(palette::trail().color()),
        );
        if let Some(last) = facts.state.and_then(|f| f.last_touched) {
            ui.label(dim(format!(
                "last {} ago",
                format::duration(snapshot.at.saturating_duration_since(last))
            )));
        }
        if facts.state.is_some_and(|f| f.deleted) {
            ui.label(RichText::new("deleted").color(palette::contention().color()));
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.label(dim("verified"));
        if facts.verified() {
            ui.label(
                RichText::new("yes — tests ran after the last change")
                    .color(palette::done_verified().color()),
            );
        } else if facts.state.is_some() {
            ui.label(
                RichText::new("no — this is really \"needs review\"")
                    .color(palette::done_unverified().color()),
            );
        } else {
            ui.label(dim("nothing has changed it"));
        }
    });

    // "Fuzzy above, exact below": a definite list, with times.
    ui.label(heading("TOUCHED BY"));
    if facts.touches.is_empty() {
        ui.label(dim(if facts.on_map {
            "nobody — the map is drawing this building at rest"
        } else {
            "nobody"
        }));
    }
    for touch in &facts.touches {
        let response = ui
            .horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(
                    RichText::new(&touch.label)
                        .small()
                        .color(palette::worker().color()),
                );
                match touch.status {
                    Some(status) => {
                        ui.label(
                            RichText::new(status_word(status).trim())
                                .small()
                                .monospace()
                                .color(palette::status(status).color()),
                        );
                    }
                    None => {
                        ui.label(dim("ended")).on_hover_text(
                            "This thread is no longer in the world; the file still \
                             records that it was here.",
                        );
                    }
                }
                if touch.visits > 0 {
                    ui.label(dim(format!("{}× ({} writes)", touch.visits, touch.writes)));
                }
                if let Some(last) = touch.last {
                    ui.label(
                        RichText::new(format!(
                            "last {} ago",
                            format::duration(snapshot.at.saturating_duration_since(last))
                        ))
                        .small()
                        .monospace()
                        .color(palette::selection().color()),
                    );
                }
                // How long this file has been in play for this thread. Six
                // visits over forty seconds is an agent working; six over an
                // hour is one that keeps coming back — PRD §12's thrashing,
                // which a revisit count alone cannot tell you.
                if let (Some(first), Some(last)) = (touch.first, touch.last) {
                    let span = last.saturating_duration_since(first);
                    if span >= Duration::from_secs(60) {
                        ui.label(dim(format!("over {}", format::duration(span))));
                    }
                }
            })
            .response;
        if response.interact(egui::Sense::click()).clicked() {
            action.follow = Some(touch.thread.clone());
        }
    }

    ui.horizontal(|ui| {
        ui.label(heading("RECENT OPERATIONS"));
        if facts.ops_total > facts.ops.len() {
            ui.label(dim(format!("{} of {}", facts.ops.len(), facts.ops_total)));
        }
    });
    if facts.ops.is_empty() {
        ui.label(dim("none in the window the world keeps"));
    }
    for op in &facts.ops {
        // One line, never two. The rail is 280–620 points wide and a monospace
        // row that wraps turns a twelve-row list into a twenty-four-row wall —
        // which is exactly what the first version did on the live window,
        // because it printed the thread's full title on every row. *Who* here is
        // the **worker**, which is the question a file with a dozen subagents on
        // it actually raises; the thread is a row above, under TOUCHED BY.
        let who = op.worker.as_ref().map_or_else(
            || clip(&op.thread, 10),
            |worker| drill::short_id(worker.as_str()),
        );
        ui.label(
            RichText::new(format!(
                "{:>7} ago {:<11} {:<7} {who}",
                format::duration(snapshot.at.saturating_duration_since(op.at)),
                clip(op.tool.name(), 11),
                outcome_word(op.outcome),
            ))
            .small()
            .monospace()
            .color(palette::outcome(op.outcome).color()),
        );
    }
    action
}

/// Truncates to a column width, so a monospace row cannot wrap.
fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    text.chars()
        .take(width.saturating_sub(1))
        .chain(std::iter::once('~'))
        .collect()
}

/// The hover card: PRD §12's *"a definite list of which threads touched it and
/// when"*, at the pointer.
///
/// Deliberately the same facts as [`building_panel`], shorter. The card is what
/// the operator reads while still deciding whether to act, so it may drop rows;
/// it may not disagree with the panel about the rows it keeps.
pub fn hover_card(
    ctx: &egui::Context,
    at: egui::Pos2,
    snapshot: &WorldSnapshot,
    path: &LogicalPath,
) {
    let facts = drill::facts(snapshot, path);
    egui::Area::new(egui::Id::new("polis-hover"))
        .order(egui::Order::Tooltip)
        .fixed_pos(at + egui::Vec2::new(16.0, 16.0))
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style())
                .fill(Color32::from_rgb(14, 16, 20))
                .show(ui, |ui| {
                    ui.set_max_width(460.0);
                    ui.label(
                        RichText::new(path.as_str())
                            .monospace()
                            .color(palette::selection().color()),
                    );
                    for mark in &facts.marks {
                        ui.label(
                            RichText::new(format!(
                                "{} · {}",
                                drill::word(&mark.kind),
                                detail(&mark.kind)
                            ))
                            .small()
                            .strong()
                            .color(drill::ink(&mark.kind).color()),
                        );
                    }
                    if facts.untouched() {
                        ui.label(dim("no channel has reported on this file"));
                    } else {
                        let (added, removed) = facts.diff();
                        ui.label(
                            RichText::new(format!(
                                "+{added} −{removed} · {} reads · {} writes{}",
                                facts.state.map_or(0, |f| f.reads),
                                facts.state.map_or(0, |f| f.writes),
                                if facts.verified() { " · verified" } else { "" }
                            ))
                            .small()
                            .monospace()
                            .color(palette::trail().color()),
                        );
                    }
                    for touch in facts.touches.iter().take(6) {
                        let when = touch.last.map_or_else(
                            || "—".to_owned(),
                            |last| {
                                format!(
                                    "{}× · last {} ago",
                                    touch.visits,
                                    format::duration(snapshot.at.saturating_duration_since(last))
                                )
                            },
                        );
                        ui.label(
                            RichText::new(format!("{} · {when}", touch.label))
                                .small()
                                .color(palette::worker().color()),
                        );
                    }
                    if facts.touches.len() > 6 {
                        ui.label(dim(format!("and {} more", facts.touches.len() - 6)));
                    }
                    if let Some(op) = facts.ops.first() {
                        ui.label(
                            RichText::new(format!(
                                "last: {} {} — {} ago",
                                op.tool.name(),
                                outcome_word(op.outcome),
                                format::duration(snapshot.at.saturating_duration_since(op.at))
                            ))
                            .small()
                            .monospace()
                            .color(palette::outcome(op.outcome).color()),
                        );
                    }
                    ui.label(
                        RichText::new("click to open in your editor")
                            .small()
                            .color(palette::status(ThreadStatus::Idle).color()),
                    );
                });
        });
}

/// A contention end, named as precisely as the channels allow: the thread, and
/// the worker inside it when there was one.
fn actor_label(actor: &polis_world::contention::Actor) -> String {
    match &actor.worker {
        Some(worker) => format!(
            "{}/{}",
            drill::short_id(actor.thread.as_str()),
            drill::short_id(worker.as_str())
        ),
        None => format!("{} (main)", drill::short_id(actor.thread.as_str())),
    }
}

/// PRD §10.2's three outcomes as words, so the column reads without colour.
fn outcome_word(outcome: polis_events::Outcome) -> &'static str {
    match outcome {
        polis_events::Outcome::Pending => "pending",
        polis_events::Outcome::Done => "done",
        polis_events::Outcome::Failed => "FAILED",
    }
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

/// What the city is, as the `h` sheet's first block and the overlay's body.
///
/// The keys come second deliberately: someone pressing `h` on a map they cannot
/// read wants to know what a building is before they want to know what `[` does.
pub fn what_you_are_looking_at(ui: &mut egui::Ui) {
    ui.label(heading("WHAT YOU ARE LOOKING AT"));
    // One paragraph, re-wrapped by egui: the rail is 280–620 points wide and the
    // overlay is wider still, so the terminal's 78-column line breaks would be
    // a ragged edge in the middle of the measure.
    ui.label(
        RichText::new(crate::explain::paragraph())
            .color(palette::worker().color())
            .line_height(Some(18.0)),
    );
}

/// The same thing as a two-column legend, for the surfaces with room for one.
///
/// `horizontal_wrapped`, not `horizontal`: a label in a plain horizontal layout
/// does not wrap, and the rail is narrow enough that the second column would be
/// clipped at exactly the width where it stops being readable.
pub fn map_legend(ui: &mut egui::Ui) {
    for (name, what) in crate::explain::READING {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(
                RichText::new(*name)
                    .monospace()
                    .color(palette::hover().color()),
            );
            ui.label(dim(*what));
        });
    }
}

/// Shape is the operation, colour is how it went (PRD §10.1, §10.2).
///
/// The glyph half already existed and answered half the question: an operator
/// seeing a barred circle turn red has been told what the shape means and never
/// what the colour means.
pub fn notation(ui: &mut egui::Ui) {
    ui.label(heading("NOTATION"));
    ui.label(dim("shape is the operation"));
    crate::mapview::legend(ui);
    ui.add_space(4.0);
    ui.label(dim("colour is how it went"));
    for (outcome, what) in [
        (
            polis_events::Outcome::Pending,
            "in flight, or no result yet",
        ),
        (polis_events::Outcome::Done, "it succeeded"),
        (polis_events::Outcome::Failed, "it failed, or was refused"),
    ] {
        ui.horizontal(|ui| {
            let (rect, _) =
                ui.allocate_exact_size(egui::Vec2::new(16.0, 14.0), egui::Sense::hover());
            ui.painter()
                .circle_filled(rect.center(), 4.0, palette::outcome(outcome).color());
            ui.label(dim(what));
        });
    }
}

/// The key sheet, under the map explanation it exists to be found from.
pub fn help(ui: &mut egui::Ui) {
    what_you_are_looking_at(ui);
    ui.add_space(4.0);
    map_legend(ui);
    ui.separator();
    ui.label(heading("KEYS"));
    for (key, what) in [
        ("drag / arrows", "pan"),
        ("scroll / + -", "zoom"),
        (
            "t",
            "swap the map and the filesystem tree — the same selection in both",
        ),
        (
            "click a file",
            "select it and open it in your editor — map row or tree row",
        ),
        (
            "hover a file",
            "who touched it, when, and what is contending for it",
        ),
        ("a", "jump to the next thing waiting on you (worst first)"),
        (
            "f",
            "follow the next thread — cut, never pan; again to move on",
        ),
        ("r", "reset the camera"),
        ("s", "the streets layer — which files import which"),
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
    notation(ui);
}

/// The first-run overlay: what the map is, over the map itself.
///
/// Returns true when the operator dismissed it this frame — any key, any click.
/// The scrim is a full-screen click target so the dismissing click cannot also
/// land on a building and open an editor, and [`crate::app`] drops the frame's
/// keys for the same reason.
pub fn explainer(ctx: &egui::Context) -> bool {
    let screen = ctx.viewport_rect();
    let mut clicked = false;
    egui::Area::new(egui::Id::new("polis-explainer"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            // The scrim is the whole viewport and it is a click target, so the
            // dismissing click cannot also land on a building underneath.
            let (rect, response) = ui.allocate_exact_size(screen.size(), egui::Sense::click());
            ui.painter()
                .rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(4, 5, 7, 205));
            clicked = response.clicked();

            // The card's rectangle is computed rather than anchored. An
            // `Area::anchor` centres using the size the area had *last* frame,
            // and this window is deliberately idle — with nothing playing, the
            // correcting frame never comes and the card sits with its corner at
            // the centre of the screen, half of it off the edge.
            let width = (rect.width() - 96.0).clamp(300.0, 620.0);
            let top = rect.top() + (rect.height() * 0.12).max(16.0);
            let card = egui::Rect::from_min_size(
                egui::Pos2::new(rect.center().x - width / 2.0, top),
                egui::Vec2::new(width, (rect.bottom() - top - 24.0).max(120.0)),
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(card), |ui| {
                egui::Frame::popup(ui.style())
                    .fill(Color32::from_rgb(14, 16, 20))
                    .inner_margin(20.0)
                    .show(ui, |ui| {
                        ui.set_max_width(width - 40.0);
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new("P O L I S")
                                        .monospace()
                                        .size(20.0)
                                        .color(palette::selection().color()),
                                );
                                ui.label(dim(
                                    "your coding agents, drawn as a city seen from above",
                                ));
                                ui.add_space(12.0);
                                what_you_are_looking_at(ui);
                                ui.add_space(12.0);
                                notation(ui);
                                ui.add_space(14.0);
                                ui.label(
                                    RichText::new(crate::explain::DISMISS)
                                        .color(palette::hover().color()),
                                );
                            });
                    });
            });
        });
    clicked
        || ctx.input(|i| {
            i.pointer.any_pressed()
                || i.events
                    .iter()
                    .any(|e| matches!(e, egui::Event::Key { pressed: true, .. }))
        })
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

    /// A pass over a viewport of a known size. `RawInput::screen_rect`'s default
    /// is 10 000 x 10 000, which is not a window anybody has.
    fn raw_input(keys: &[egui::Key]) -> egui::RawInput {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::new(1400.0, 900.0));
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
        input
    }

    /// The blocker: nothing in the window ever explained the map. It has to
    /// draw, and it has to go away on one keystroke — a modal that needs to be
    /// hunted for a close button is worse than no modal.
    #[test]
    fn the_explainer_draws_and_any_key_dismisses_it() {
        let ctx = egui::Context::default();
        let mut dismissed = true;
        let mut full = ctx.run_ui(raw_input(&[]), |ui| {
            dismissed = explainer(ui.ctx());
        });
        assert!(!dismissed, "nothing was pressed, so it stays up");
        assert!(!full.shapes.is_empty(), "it drew nothing at all");
        // `epaint` panics if a texture delta is dropped unapplied.
        full.textures_delta.clear();

        let mut full = ctx.run_ui(raw_input(&[egui::Key::Space]), |ui| {
            dismissed = explainer(ui.ctx());
        });
        full.textures_delta.clear();
        assert!(dismissed, "a keystroke clears it");

        // And `h` reaches the same words from the sheet, forever after.
        let mut full = ctx.run_ui(raw_input(&[]), help);
        full.textures_delta.clear();
    }

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
