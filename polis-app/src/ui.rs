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
use polis_render::live::CloudCensus;
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
    /// Whether the legend is up, over the map.
    ///
    /// It carries what used to be a fold on **every thread card** — PRD §12's
    /// *why it looks like this*. Per-card it cost three lines of a column that
    /// is now fifteen percent of the window, and it said the same thing about
    /// notation nine times over; here it is one panel, opened from the map's own
    /// corner, that answers for the selected thread and for the notation at
    /// once.
    pub legend: bool,
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
            legend: false,
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
    /// What the cloud layer drew, and what it withheld.
    ///
    /// The whole struct rather than `(shown, kernels)`, because the pair could
    /// only ever say `0 clouds (0 kernels)` and the operator's question is the
    /// next one.
    pub clouds: CloudCensus,
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

        // Beside `off-repo paths` because it is the same kind of admission —
        // evidence the map received and did not use — and because the two of
        // them together are the whole answer to *"why has this thread no
        // cloud"*. A shell call's only path signal is its `cwd`, and a `cwd` at
        // the checkout root is the absorbing element of PRD §6.2's ancestor, so
        // it is counted and then dropped. Without this line the drop is
        // invisible: the operation still draws its mark and still counts in the
        // thread's own total, so more than half the evidence stream can go
        // nowhere with nothing on any surface saying so.
        if health.root_scoped_observations > 0 {
            ui.label(dim(format!(
                "root-scoped {}",
                health.root_scoped_observations
            )))
            .on_hover_text(
                "Observations whose only path was the checkout root — mostly \
                 shell calls run from the top of the tree. The whole city is not \
                 a location (PRD §6.2), so they feed no territory and drop no \
                 kernel. A session that is nearly all root-scoped will not \
                 converge, and will have no cloud.",
            );
        }

        // The one counter here that is about the notation rather than the
        // channels, and it is here for the same reason the others are: the
        // operator's report was *"the same color is super bad"*, and past twelve
        // threads two rows share a hue again. Without this line that is a
        // regression the operator has to re-report; with it, it is a reading.
        if health.identity_hues_exhausted > 0 {
            ui.label(dim(format!(
                "shared hues {}",
                health.identity_hues_exhausted
            )))
            .on_hover_text(
                "There are twelve identity hues (PRD §11.4) and this world has \
                 seen more than twelve threads, so this many of them fell back \
                 to a colour some thread has already held. A slot is never \
                 recycled, so the twelve may include threads long since retired \
                 — the rows on screen right now can still all be distinct. The \
                 rail's name is what tells them apart until Polis is restarted.",
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
            // `CloudCensus::reason` and not `{shown} clouds`. The bar used to
            // read `0 clouds (0 kernels)`, which states the symptom twice and
            // the cause not at all; `0 CLOUDS · 4 UNCONVERGED` names the gate
            // that fired, in `polis_world::territory::select_clouds`'s own
            // vocabulary, and `6 CLOUDS · 0.8 PX: ZOOM IN` names the one gate
            // that is not in `polis-world` at all.
            let clouds = if vitals.clouds.withheld() || vitals.clouds.sub_pixel() {
                vitals.clouds.reason()
            } else {
                format!("{} clouds", vitals.clouds.shown)
            };
            ui.label(dim(format!(
                "labels {}+{} dropped · {} buildings · {clouds} · {} kernels",
                vitals.labels.0, vitals.labels.1, vitals.buildings, vitals.clouds.kernels,
            )))
            .on_hover_text(
                "Every thread lands in exactly one bucket, so the counts sum to the rail's row \
                 count. UNCONVERGED: PRD §6.2's evidence does not agree yet, or it agrees about \
                 ground this city has no geometry for. DORMANT: quiet past PRD §10.4's window. \
                 OVER CAP: ranked out by the cloud cap. ZOOM IN: the clouds were drawn and are \
                 narrower than a stroke at this camera — the one reason that is not the world's.",
            );
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
            //
            // Three counts, not one. `Convergence::observations` is
            // `evidence.len()` — the *live, undecayed-out, non-root* window that
            // PRD §6.2's gates actually run over — and printing it alone said
            // "No territory yet: 0 observations" about a thread that had made
            // 417 tool calls, which reads as a broken ingest rather than as a
            // thread whose evidence has decayed. `Territory::observations` is
            // the undecayed total the operator recognises, and the gap between
            // the two is the answer: a large total with an empty window is a
            // thread whose work says nothing about where.
            let c = thread.territory.convergence();
            ui.label(
                RichText::new("unplaced")
                    .small()
                    .color(palette::needs_decision().color()),
            )
            .on_hover_text(format!(
                "No territory yet. {} tool calls seen in all, of which {} are still live \
                 evidence; PRD §6.3 decays the rest with a 90 s half-life, and a shell call at \
                 the checkout root never becomes evidence at all. Of that live window: ancestor \
                 depth {}, mass ratio {:.2}, and no cluster heavy enough to be a lobe. PRD §6.2 \
                 emits at depth ≥ 2 and mass > 0.70; PRD §6.4's lobes need {:.0}% of the weight \
                 in one place.",
                thread.territory.observations,
                thread.territory.evidence.len(),
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
    phrases: &crate::intent::Captions,
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
        if let Some(closed) = thread_card(ui, snapshot, state, thread, phrases.get(&thread.id)) {
            dismissed = Some(closed);
        }
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

/// Text lines in one card. Fixed, not derived from the content.
///
/// The column is fifteen percent of the window by default, so a card that grew
/// with its thread would make the list unreadable exactly when there is most to
/// read: nine cards of nine different heights cannot be scanned, and an operator
/// scans this list before reading any single row of it. Three lines, every row,
/// and anything longer is truncated into the hover.
const CARD_LINES: f32 = 3.0;

/// Width of the tint bar down a card's left edge.
const CARD_BAR: f32 = 4.0;

/// Padding inside a card, on every side.
const CARD_PAD: f32 = 5.0;

/// One thread, as three fixed lines with its own colour down the side.
///
/// Painted rather than assembled from widgets, because the two things the card
/// owes — an exact height, and a bar the *full* height of it — are both
/// properties of a rectangle that has to be known before the text goes into it,
/// and `Ui::horizontal` decides its height afterwards.
///
/// # The bar is always drawn, and that is the point
///
/// It used to appear only under hover or selection, which meant the thing PRD
/// §11.4 makes identity out of — the thread's own hue — was invisible until the
/// operator was already pointing at the row they wanted. A cloud on the map and
/// its card in the list now carry the same colour at all times, so the match is
/// made by looking rather than by hunting.
///
/// Returns the thread if the operator closed it.
fn thread_card(
    ui: &mut egui::Ui,
    snapshot: &WorldSnapshot,
    state: &mut ViewState,
    thread: &Thread,
    phrase: Option<&crate::intent::Caption>,
) -> Option<ThreadId> {
    let ink = palette::status(thread.status);
    let tint = thread.tint;
    let following = state.follow.as_ref() == Some(&thread.id);
    let selected = state.selected_thread.as_ref() == Some(&thread.id);

    let small = egui::TextStyle::Small.resolve(ui.style());
    let body = egui::TextStyle::Body.resolve(ui.style());
    let line = ui.text_style_height(&egui::TextStyle::Body).max(14.0);
    let height = line.mul_add(CARD_LINES, CARD_PAD * 2.0);

    let (rect, hit) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::click(),
    );
    let text_left = rect.left() + CARD_BAR + CARD_PAD;
    // Room kept clear on the first line for the close control, which is a
    // destructive verb and must not be found by an operator reaching for
    // "follow this one".
    let close = egui::Rect::from_min_size(
        egui::pos2(rect.right() - CARD_PAD - line, rect.top() + CARD_PAD),
        egui::vec2(line, line),
    );
    let text_right = close.left() - CARD_PAD;
    let width = (text_right - text_left).max(1.0);

    if ui.is_rect_visible(rect) {
        if selected || state.emphasises(&thread.id) || hit.hovered() {
            let wash = if selected { 0.14 } else { 0.09 };
            ui.painter()
                .rect_filled(rect, 2.0, palette::thread(tint).alpha(wash));
        }
        // The bar, the full height of the card (PRD §11.4: an edge survives
        // greyscale and the periphery, where a colour difference does not).
        ui.painter().rect_filled(
            egui::Rect::from_min_max(rect.min, egui::pos2(rect.min.x + CARD_BAR, rect.max.y)),
            0.0,
            palette::thread(tint).alpha(if selected || hit.hovered() { 1.0 } else { 0.8 }),
        );

        let mut y = rect.top() + CARD_PAD;
        // Line one — the state word, and how long ago this thread last did
        // anything. One meaning for the time column in every card: a working
        // thread reads seconds, and a thread that has gone quiet says how long.
        let age = format::duration(snapshot.at.saturating_duration_since(thread.last_activity));
        let stamp = ui
            .painter()
            .layout_no_wrap(age, small.clone(), palette::worker().color());
        ui.painter().galley(
            egui::pos2(
                text_right - stamp.size().x,
                y + (line - stamp.size().y) / 2.0,
            ),
            stamp.clone(),
            palette::worker().color(),
        );
        clipped_line(
            ui,
            egui::pos2(text_left, y),
            (width - stamp.size().x - CARD_PAD).max(1.0),
            line,
            status_word(thread.status).trim(),
            small.clone(),
            ink.color(),
        );

        // Line two — what this agent is, in its own words when it has given us
        // any.
        //
        // A phrase from `crate::intent` **replaces** the name rather than
        // sitting under it, and that is the whole reason the line is worth
        // having: `thread_label` is the `ai-title`, written from the session's
        // first prompt and never revised, so on a thread that has been running
        // for an hour it names something that finished long ago. A phrase is
        // about the last few minutes. Two answers to one question on one card
        // would leave the operator working out which is current, which is the
        // lookup the phrase exists to remove.
        //
        // A stale phrase still shows, dimmed — see `intent::Caption::is_stale`.
        // Blanking it would trade a line the operator can see is old for no line
        // at all. This is where the map's cloud captions went: the naming
        // belongs in the column that has room for it, and the map keeps the
        // line pointing at the shape (`mapview::draw_thread_connectors`).
        y += line;
        let outdated = phrase.is_some_and(|p| {
            p.is_stale(&polis_repo::llm::cache::Sketch::build(
                thread.intents.iter().map(|i| i.text.as_str()),
            ))
        });
        clipped_line(
            ui,
            egui::pos2(text_left, y),
            width,
            line,
            &phrase.map_or_else(|| crate::mapview::thread_label(thread), |p| p.text.clone()),
            body,
            if outdated {
                palette::selection().alpha(0.55)
            } else {
                palette::selection().color()
            },
        );

        // Line three — where it is working, or what it is waiting on when that
        // is the more urgent of the two. `why_of` answers "why", which is the
        // question an operator asks of a thread that has stopped.
        y += line;
        let blocked = why_of(snapshot, thread);
        let detail = blocked.clone().unwrap_or_else(|| place_word(thread));
        clipped_line(
            ui,
            egui::pos2(text_left, y),
            width,
            line,
            &detail,
            small,
            if blocked.is_some() {
                ink.color()
            } else {
                palette::worker().color()
            },
        );

        if following || hit.hovered() {
            ui.painter().text(
                close.center(),
                egui::Align2::CENTER_CENTER,
                if following { "◉" } else { "✕" },
                egui::TextStyle::Small.resolve(ui.style()),
                palette::status(ThreadStatus::Idle).color(),
            );
        }
    }

    if hit.hovered() {
        // PRD §12's shared highlight, written from the list; the map lights that
        // thread's marks up when it reads it.
        state.hover_thread(&thread.id);
    }
    // Where this card ended up, for the line the map draws from it to this
    // thread's cloud. Clipped to what the list is actually showing, so a card
    // scrolled out of view anchors nothing.
    state.rail_card(&thread.id, rect, ui.clip_rect());

    // The close control is a separate target inside the card, claimed before the
    // card's own click is read.
    let closed = ui
        .interact(
            close,
            ui.id().with(("close", thread.id.as_str())),
            egui::Sense::click(),
        )
        .on_hover_text(
            "Close this thread. Nothing is remembered, so its next event brings it back.",
        )
        .clicked();
    if closed {
        return Some(thread.id.clone());
    }

    if hit.clicked() {
        // One click does both verbs, because on this card they are one
        // intention: "this thread". Follow owns the camera and selection owns
        // the highlight, but the operator is not asking for two things and must
        // not have to put them away separately. The second click on the same
        // card clears both — a mark that can only be moved and never dismissed
        // is a mark the operator stops trusting.
        let asked_about = following || selected;
        let (follow, chosen) = if asked_about {
            (None, None)
        } else {
            (Some(thread.id.clone()), Some(thread.id.clone()))
        };
        state.follow = follow;
        state.selected_thread = chosen;
    }
    // Everything the card had to truncate. The list is for scanning; this is for
    // the one row the operator stopped on.
    hit.on_hover_text(format!(
        "{}\n{} · {} calls · {} fail · +{} -{}",
        crate::mapview::thread_label(thread),
        place_word(thread),
        thread.tool_calls,
        thread.failures,
        thread.lines_added,
        thread.lines_removed
    ));
    None
}

/// One line of a card, truncated to the width it was given.
///
/// `max_rows = 1` with `break_anywhere`, so a long title or a long district ends
/// in an ellipsis instead of pushing the card past [`CARD_LINES`].
fn clipped_line(
    ui: &egui::Ui,
    at: egui::Pos2,
    width: f32,
    line: f32,
    text: &str,
    font: egui::FontId,
    colour: Color32,
) {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, colour);
    job.wrap.max_width = width;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(
        egui::pos2(at.x, at.y + (line - galley.size().y) / 2.0),
        galley,
        colour,
    );
}

/// Where a thread is, in the few words a card has room for.
///
/// The rail used to spend a line and a hover on this ([`place_of`], which the
/// legend still uses in full). A card gets part of one line, so §6.4's lobes
/// become the heaviest one and a count, and §6.2's refusal to place a thread
/// stays the honest word it has always been rather than becoming a guess.
fn place_word(thread: &Thread) -> String {
    use polis_world::territory::Placement;

    match thread.territory.placement() {
        Placement::Claim(claim) => format!("in {}", claim.as_str()),
        Placement::Lobes(lobes) => {
            let named = lobes.first().map_or("", |l| l.path.as_str());
            match lobes.len() {
                0 | 1 => format!("in {named}"),
                n => format!("in {named} +{}", n - 1),
            }
        }
        Placement::Nowhere => "unplaced".to_owned(),
    }
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

/// The legend, over the map: what the notation means, and why the selected
/// thread looks the way it does.
///
/// # One panel instead of a fold on every card
///
/// *Why it looks like this* used to be a `CollapsingHeader` on **each** thread
/// row. That was the right content in the wrong place three times over: it
/// pushed every card past the height a list can be scanned at, it repeated the
/// same notation once per thread, and it was closed by default — so the reading
/// that explains a colour was hidden behind a click on a row the operator had
/// already decided not to care about.
///
/// Here it is one panel, reached from the map's own corner or from `h`, and it
/// answers for whichever thread is selected. Selection is already the thing that
/// binds the list, the map and the terminal pane together (ADR-0105), so the
/// legend follows it for free and there is nothing new to keep in sync.
///
/// `open` is egui's own window flag, so the title bar's `✕` closes it and the
/// caller's toggle and the window agree without a second piece of state.
pub fn legend(
    ctx: &egui::Context,
    open: &mut bool,
    snapshot: &WorldSnapshot,
    selected: Option<&ThreadId>,
    captions: &crate::intent::Captions,
    look: &mut crate::config::Look,
    adapter: &str,
) {
    egui::Window::new("legend")
        .open(open)
        .default_width(420.0)
        .max_height(620.0)
        .resizable(true)
        .collapsible(false)
        .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -16.0))
        .show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let thread = selected.and_then(|id| snapshot.thread(id));
                    if let Some(thread) = thread {
                        selected_thread_reading(ui, snapshot, thread, captions);
                        ui.separator();
                    } else {
                        ui.label(dim("select an agent to read why it looks the way it does"));
                        ui.separator();
                    }
                    // The key sheet lives here now, not in a column of its
                    // own. `h` used to open it inside the agent list, which is
                    // fifteen percent of the window — a two-column table of
                    // every keystroke in a space narrower than the table. It is
                    // the same question this panel already answers, so it is the
                    // same panel: the map's corner, the title bar and `h` are
                    // three doors into one room.
                    look_controls(ui, look);
                    ui.separator();
                    help(ui);
                    ui.separator();
                    ui.label(
                        RichText::new(adapter)
                            .small()
                            .monospace()
                            .color(palette::worker().color()),
                    );
                });
        });
}

/// The two things about the cloud layer that are the operator's call, next to
/// the panel that explains what the layer means.
///
/// # Why these two are settings and nothing else here is
///
/// Every other number in the notation was settled by measurement — the iso
/// thresholds against real sessions, the hatch spacing against an ink budget,
/// the identity ring against CIEDE2000 at the luminance each role is drawn at.
/// These two cannot be, because they are trades between things the code has no
/// way to rank:
///
/// * **cloud body** trades the city's own legibility for the cloud's. A dense
///   core is the shape that survives being seen from across the room (PRD §1)
///   and it is also the shape that hides the buildings under it (PRD §10.3).
///   Which one an operator wants depends on whether they are reading the map or
///   watching it, and that changes through the day.
/// * **rail points at cloud** trades quiet for naming. One line answers *which
///   thread is that* when asked; a line per thread answers it before it is
///   asked, at the cost of several lines across the window.
///
/// So they are here, live, next to the picture they change — a slider the
/// operator can drag while looking at the map is worth more than a number
/// somebody else measured once.
fn look_controls(ui: &mut egui::Ui, look: &mut crate::config::Look) {
    use crate::config::Connectors;
    ui.label(heading("LOOK"));
    ui.horizontal(|ui| {
        ui.label(dim("cloud body"));
        ui.add(
            egui::Slider::new(&mut look.cloud_veil, 0.0..=1.0)
                .fixed_decimals(2)
                .show_value(true),
        )
        .on_hover_text(
            "How much of the city a cloud may hide. 0 draws contour and hatch over an \
             untouched map; 1 gives each thread a core dense enough to read from across \
             the room, and the buildings under it are gone.",
        );
    });
    ui.horizontal(|ui| {
        ui.label(dim("rail points at cloud"));
        for mode in [Connectors::Off, Connectors::Asked, Connectors::Always] {
            if ui
                .selectable_label(look.connectors == mode, mode.label())
                .on_hover_text(match mode {
                    Connectors::Off => "No line. A cloud is matched to its agent by hue alone.",
                    Connectors::Asked => {
                        "One line, to the cloud of the agent you are pointing at, have \
                         selected, or are following."
                    }
                    Connectors::Always => "A line from every card to its own cloud, all the time.",
                })
                .clicked()
            {
                look.connectors = mode;
            }
        }
    });
}

/// The selected thread, in full: the detail a three-line card had to truncate.
/// The model-written phrase, in full, with how old it is.
///
/// # Why it is longer here than on the map
///
/// The caption beside a cloud is cut to `intent::PHRASE_CHARS` and carries its
/// age only as a dimming — which is the right trade at map scale, where the
/// question is *"which of these four agents do I care about"* and a second line
/// per cloud is four more lines of type over the city.
///
/// This panel is where the operator has already chosen a thread and is asking
/// about it, so the phrase gets its age in words and its staleness in a
/// sentence. ADR-0089 §5 requires staleness to be *visible*, and a dimmed line
/// on a map says "old" without ever saying *how* old.
///
/// # It also says when there is nothing, and why
///
/// An operator who passed `--captions` and sees no phrase has exactly one
/// question, and every way it can happen — no key, a dead endpoint, a 429, a
/// model that declined — is a line in [`crate::intent::Captions::last_error`].
/// Printing it here is the difference between a feature that failed and a
/// feature that appears not to exist.
fn what_it_is_doing(
    ui: &mut egui::Ui,
    thread: &Thread,
    captions: &crate::intent::Captions,
    now: std::time::Instant,
) {
    let Some(caption) = captions.get(&thread.id) else {
        if captions.is_on() {
            ui.label(dim(captions.last_error().map_or_else(
                || "no phrase for this agent yet".to_owned(),
                |why| format!("no phrase — {why}"),
            )));
        }
        return;
    };
    let notes =
        polis_repo::llm::cache::Sketch::build(thread.intents.iter().map(|i| i.text.as_str()));
    ui.label(
        RichText::new(&caption.text)
            .color(palette::selection().color())
            .italics(),
    );
    let age = crate::format::duration(caption.age(now));
    ui.label(dim(if caption.is_stale(&notes) {
        format!("written {age} ago, from notes this agent has since moved past")
    } else {
        format!(
            "written {age} ago, from the agent's own notes on {} of its calls",
            thread.intents.len()
        )
    }));
    ui.add_space(4.0);
}

fn selected_thread_reading(
    ui: &mut egui::Ui,
    snapshot: &WorldSnapshot,
    thread: &Thread,
    captions: &crate::intent::Captions,
) {
    ui.horizontal(|ui| {
        swatch(ui, thread.tint);
        ui.label(
            RichText::new(status_word(thread.status).trim())
                .monospace()
                .color(palette::status(thread.status).color()),
        );
        ui.label(
            RichText::new(crate::mapview::thread_label(thread))
                .color(palette::selection().color())
                .strong(),
        );
    });
    what_it_is_doing(ui, thread, captions, snapshot.at);
    place_of(ui, thread);
    // PRD §12: thrashing — "the same building revisited six times". It has no
    // room on a card and it is exactly the kind of thing this panel is for.
    if let Some((path, count)) = thread.most_revisited() {
        if count >= 3 {
            ui.label(
                RichText::new(format!("revisited {count}× {}", short_path(path)))
                    .small()
                    .color(palette::needs_decision().color()),
            );
        }
    }
    ui.add_space(4.0);
    for reading in crate::explain::thread_reading(thread, snapshot.at) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(
                RichText::new(reading.mark)
                    .small()
                    .monospace()
                    .color(palette::hover().color()),
            );
            ui.label(
                RichText::new(reading.says)
                    .small()
                    .color(palette::selection().color()),
            );
        });
        ui.label(dim(reading.because).line_height(Some(15.0)));
        ui.add_space(6.0);
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
        (
            "o",
            "switch to another repository — every one with agents working in it",
        ),
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
    terminals(ui);
    ui.separator();
    notation(ui);
}

/// The terminal dock's own keys, and what a pane is.
///
/// Read from [`crate::panes::RESERVED`] rather than retyped, which is the whole
/// reason that table is data: a chord that works and a chord the sheet claims
/// cannot drift apart.
fn terminals(ui: &mut egui::Ui) {
    ui.separator();
    ui.label(heading("TERMINALS"));
    ui.label(dim(
        "Ctrl+Alt+T starts a Claude Code session in a pane beside the map. \
         Its agent runs in polis-sessiond, not in this window — closing the \
         window leaves it working, and opening one again puts the same \
         screen back. Clicking its tab lights its cloud on the map; \
         picking it on the map — its rail row, or `a` — brings the tab to \
         the front.",
    ));
    ui.add_space(2.0);
    for (chord, what) in crate::panes::RESERVED {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{chord:>16}"))
                    .monospace()
                    .color(palette::hover().color()),
            );
            ui.label(dim(*what));
        });
    }
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

/// The state word, padded to one column width.
///
/// `pub(crate)` because `crate::mapview`'s cloud captions say the same word
/// about the same thread, and a map and a rail that disagree about whether a
/// thread is parked or idle is the disagreement this vocabulary exists to
/// prevent.
pub(crate) fn status_word(status: ThreadStatus) -> &'static str {
    match status {
        ThreadStatus::Waiting => "WAITING    ",
        // Upper case is a budget, and only `blocks_operator` states may spend
        // it. `interrupted` is the exception that proves the column width: it is
        // the longest word the rail can say, and it earns the space because
        // "idle" for a thread the operator stopped themselves was the report
        // that widened this enum.
        ThreadStatus::Interrupted => "INTERRUPTED",
        ThreadStatus::Ready => "ready      ",
        ThreadStatus::Parked => "parked     ",
        ThreadStatus::Working => "working    ",
        ThreadStatus::Idle => "idle       ",
        ThreadStatus::Done => "done       ",
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

/// The object of the wait, for the rail's caption. `None` when the state word
/// already says everything there is to say.
///
/// Ordered by what the operator would ask next. A blocked thread names the
/// question and how long it has stood — the wait's age is the triage dimension,
/// and 63 of 112 measured `AskUserQuestion` waits ran past five minutes, so
/// "how long" separates a thread that just asked from one that has been ignored.
/// A parked thread names the job holding it, because "1 shell still running" is
/// the fact the operator was reading off their own terminal footer.
pub(crate) fn why_of(snapshot: &WorldSnapshot, thread: &Thread) -> Option<String> {
    match thread.status {
        ThreadStatus::Waiting => {
            let mark = snapshot
                .attention
                .iter()
                .find(|m| m.kind.is_decision_for(&thread.id))?;
            let AttentionKind::NeedsDecision { source, .. } = &mark.kind else {
                return None;
            };
            Some(format!(
                "{} · {}",
                format::duration(mark.waited(snapshot.at)),
                source.label()
            ))
        }
        ThreadStatus::Interrupted => thread.interrupted_at.map(|at| {
            format!(
                "{} ago · you stopped it",
                format::duration(snapshot.at.saturating_duration_since(at))
            )
        }),
        ThreadStatus::Parked => {
            let job = thread.background.first()?;
            let more = thread.background.len().saturating_sub(1);
            let label = job.label.as_deref().unwrap_or("background job");
            Some(match more {
                0 => format!(
                    "{} · {}",
                    format::duration(job.age(snapshot.at)),
                    short(label)
                ),
                n => format!(
                    "{} · {} +{n} more",
                    format::duration(job.age(snapshot.at)),
                    short(label)
                ),
            })
        }
        _ => None,
    }
}

/// A command line trimmed to something a rail row can hold.
fn short(text: &str) -> String {
    let one = text.split('\n').next().unwrap_or(text).trim();
    if one.chars().count() <= 32 {
        return one.to_owned();
    }
    let head: String = one.chars().take(31).collect();
    format!("{head}…")
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

    /// The sheet is the only place the dock's chords are written down, and
    /// `RESERVED` is data precisely so the two cannot drift. Asserted against
    /// text that reached the tessellator, not against the table it came from —
    /// a section that compiles and never draws would pass the weaker test.
    #[test]
    fn the_help_sheet_lists_every_terminal_chord() {
        let ctx = egui::Context::default();
        let mut full = ctx.run_ui(raw_input(&[]), help);
        let text: String = laid_out(&full)
            .into_iter()
            .map(|(line, _)| line)
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        full.textures_delta.clear();

        for (chord, what) in crate::panes::RESERVED {
            assert!(
                text.contains(chord),
                "the sheet never mentions {chord} ({what})"
            );
        }
        assert!(
            text.contains("TERMINALS"),
            "the terminal section did not draw"
        );
    }

    /// Every string the frame actually laid out, with where it landed.
    ///
    /// The panel's failure mode is not a wrong sentence, it is a fold that
    /// compiles and never opens — so the assertion has to be about text that
    /// reached the tessellator, not about the `Vec<Reading>` behind it, which
    /// `crate::explain`'s own tests already cover.
    fn laid_out(full: &egui::FullOutput) -> Vec<(String, egui::Pos2)> {
        fn walk(shape: &egui::Shape, out: &mut Vec<(String, egui::Pos2)>) {
            match shape {
                egui::Shape::Text(text) => out.push((text.galley.text().to_owned(), text.pos)),
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        walk(shape, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        for clipped in &full.shapes {
            walk(&clipped.shape, &mut out);
        }
        out
    }

    /// A rail with one thread in it, which is the shape both rail tests need.
    fn one_thread(name: &str) -> WorldSnapshot {
        let mut snapshot =
            WorldSnapshot::empty(std::sync::Arc::new(polis_layout::CityLayout::default()));
        let session = polis_events::SessionId::new(name);
        let mut thread = Thread::new(ThreadId::of_session(session.clone()), session, snapshot.at);
        // The rail skips a thread that has done nothing, and rightly.
        thread.tool_calls = 1;
        snapshot.threads.push(thread);
        snapshot
    }

    /// One rail pass, and the strings it laid out.
    fn rail_frame(
        ctx: &egui::Context,
        snapshot: &WorldSnapshot,
        state: &mut ViewState,
        input: egui::RawInput,
    ) -> Vec<(String, egui::Pos2)> {
        let mut full = ctx.run_ui(input, |ui| {
            status_rail(ui, snapshot, state, &crate::intent::Captions::off());
        });
        let texts = laid_out(&full);
        // `epaint` panics if a texture delta is dropped unapplied.
        full.textures_delta.clear();
        texts
    }

    /// A press and a release on one point, which is what egui counts as a click.
    fn click_at(at: egui::Pos2) -> egui::RawInput {
        let mut input = raw_input(&[]);
        input.events.push(egui::Event::PointerMoved(at));
        for pressed in [true, false] {
            input.events.push(egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
        }
        input
    }

    /// The operator's report was *"this is all a bit cryptic"* about four marks
    /// at once. That answer used to be a fold on **every** card; it is one
    /// legend now (ADR-0108), so what has to hold is that the legend reads the
    /// *selected* thread. A legend that explained the notation but not the agent
    /// in front of the operator would be the fold's content with its point
    /// removed, and the cryptic marks would be back.
    #[test]
    fn the_legend_reads_the_selected_thread() {
        let ctx = egui::Context::default();
        ctx.all_styles_mut(|style| style.animation_time = 0.0);
        let snapshot = one_thread("explained");
        let id = snapshot.threads[0].id.clone();
        let mut look = crate::config::Look::default();
        let mut open = true;

        // A window is an `Area`, and an area has no size until it has been laid
        // out once — so the first pass places it and the second is the one with
        // text in it. Reading the first would be asserting egui's bootstrap.
        let mut frame = |selected: Option<&ThreadId>| {
            let mut texts = Vec::new();
            for _ in 0..2 {
                let mut full = ctx.run_ui(raw_input(&[]), |ui| {
                    legend(
                        ui.ctx(),
                        &mut open,
                        &snapshot,
                        selected,
                        &crate::intent::Captions::off(),
                        &mut look,
                        "adapter",
                    );
                });
                texts = laid_out(&full);
                full.textures_delta.clear();
            }
            texts
        };

        // Nothing selected: it still explains the map, and says what it is
        // waiting for rather than showing an empty panel.
        let idle = frame(None);
        assert!(
            idle.iter()
                .any(|(text, _)| text.contains("select an agent")),
            "with nothing selected the legend said nothing about what it wants: {:?}",
            idle.iter().map(|(t, _)| t).collect::<Vec<_>>()
        );

        // Selected: the readings a three-line card has no room for.
        let read = frame(Some(&id));
        for mark in ["the cloud", "the ring in the middle", "the arrow"] {
            assert!(
                read.iter().any(|(text, _)| text == mark),
                "{mark:?} is not in the legend: {:?}",
                read.iter().map(|(t, _)| t).collect::<Vec<_>>()
            );
        }
    }

    /// The map cuts the phrase to fit beside a cloud and says its age only by
    /// dimming. This panel is where the operator has already picked a thread, so
    /// the phrase is whole and its age is in words — and ADR-0089 §5 requires
    /// staleness to be visible, which a dimmed line on a map never quite is.
    #[test]
    fn the_legend_says_what_the_selected_agent_is_doing_and_how_old_that_is() {
        let ctx = egui::Context::default();
        ctx.all_styles_mut(|style| style.animation_time = 0.0);
        let mut snapshot = one_thread("doing");
        let id = snapshot.threads[0].id.clone();
        let notes = ["Rewrite the token refresh", "Run the auth tests"];
        for note in notes {
            snapshot.threads[0].intents.push_back(polis_world::Intent {
                tool: polis_events::ToolKind::Bash,
                text: note.to_owned(),
                at: snapshot.at,
            });
        }
        let mut look = crate::config::Look::default();
        let mut open = true;

        let mut frame = |captions: &crate::intent::Captions| {
            let mut texts = Vec::new();
            for _ in 0..2 {
                let mut full = ctx.run_ui(raw_input(&[]), |ui| {
                    legend(
                        ui.ctx(),
                        &mut open,
                        &snapshot,
                        Some(&id),
                        captions,
                        &mut look,
                        "adapter",
                    );
                });
                texts = laid_out(&full);
                full.textures_delta.clear();
            }
            texts
        };

        // A phrase written from the notes this thread still has: whole, with an
        // age, and saying where the words came from.
        let fresh = frame(&crate::intent::Captions::seeded(
            id.clone(),
            "rewriting the token refresh",
            &notes,
        ));
        let words = |texts: &[(String, egui::Pos2)]| {
            texts.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>()
        };
        assert!(
            fresh
                .iter()
                .any(|(t, _)| t == "rewriting the token refresh"),
            "the phrase is not in the legend: {:?}",
            words(&fresh)
        );
        assert!(
            fresh
                .iter()
                .any(|(t, _)| t.contains("written") && t.contains("own notes")),
            "the phrase has no age and no provenance: {:?}",
            words(&fresh)
        );

        // A phrase written from notes the thread has since moved past is still
        // shown — blanking it would trade a line the operator can see is old for
        // no line at all — and it says so.
        let stale = frame(&crate::intent::Captions::seeded(
            id.clone(),
            "rewriting the token refresh",
            &["something", "else", "entirely", "and", "more"],
        ));
        assert!(
            stale
                .iter()
                .any(|(t, _)| t == "rewriting the token refresh"),
            "a stale phrase was blanked: {:?}",
            words(&stale)
        );
        assert!(
            stale.iter().any(|(t, _)| t.contains("moved past")),
            "a stale phrase did not say it was stale: {:?}",
            words(&stale)
        );

        // Off: the panel says nothing about captions at all, rather than
        // explaining a feature the operator did not ask for.
        let off = frame(&crate::intent::Captions::off());
        assert!(
            !off.iter().any(|(t, _)| t.contains("written")),
            "the panel talked about a feature that is off: {:?}",
            words(&off)
        );
    }

    /// The card is a fixed three lines whatever the thread has to say, because
    /// a list of nine different heights cannot be scanned — and scanning is what
    /// the column is for. Asserted on the geometry rather than on the text: a
    /// card that wrapped instead of truncating would still contain every word.
    #[test]
    fn a_card_is_the_same_height_however_much_the_thread_has_to_say() {
        let ctx = egui::Context::default();
        ctx.all_styles_mut(|style| style.animation_time = 0.0);

        let height = |title: Option<&str>| {
            let mut snapshot = one_thread("sized");
            snapshot.threads[0].title = title.map(str::to_owned);
            let mut state = ViewState::default();
            let texts = rail_frame(&ctx, &snapshot, &mut state, raw_input(&[]));
            // The two lines the card always draws: the state word, and the line
            // under it. Their spacing is the card's line height, and the card is
            // `CARD_LINES` of them plus padding.
            let top = texts
                .iter()
                .find(|(t, _)| t == "working")
                .map(|(_, p)| p.y)
                .expect("the state word never drew");
            let bottom = texts.iter().map(|(_, p)| p.y).fold(top, f32::max);
            bottom - top
        };

        let short = height(None);
        let long = height(Some(
            "a title long enough that it would wrap onto a second and probably \
             a third line if the card let it",
        ));
        assert!(
            (short - long).abs() < 1.0,
            "a long title changed the card's height: {short} vs {long}"
        );
    }

    /// > after clicking a thread the lines show and they never hide until i
    /// > click another thread, i want to click again to hide the lines
    ///
    /// What a click asks for is gated on [`ViewState::interrogates`], and the
    /// row's click used to re-assert the selection every time — so the only way
    /// out of a thread's marks was to put them onto a different thread. Both
    /// verbs now go away together, and this asserts the second click rather
    /// than the first, because the first one was never the broken half.
    ///
    /// The lines the operator was complaining about here are gone entirely —
    /// asking about a thread brightens its cloud instead
    /// (`crate::clouds::LIT_GAIN`) — but the toggle is the same one, and a
    /// highlight that cannot be put away is a highlight the operator stops
    /// trusting.
    #[test]
    fn a_second_click_on_the_same_row_puts_the_lines_away() {
        let ctx = egui::Context::default();
        ctx.all_styles_mut(|style| style.animation_time = 0.0);

        let snapshot = one_thread("toggled");
        let id = snapshot.threads[0].id.clone();
        let mut state = ViewState::default();

        let first = rail_frame(&ctx, &snapshot, &mut state, raw_input(&[]));
        let row = first
            .iter()
            .find(|(text, _)| text.starts_with("thread "))
            .map(|(_, pos)| *pos)
            .expect("the row never drew its own name");
        assert!(
            !state.interrogates(&id),
            "nothing has been asked about yet, so nothing is drawn for it"
        );

        let at = row + egui::Vec2::new(4.0, 4.0);
        rail_frame(&ctx, &snapshot, &mut state, click_at(at));
        assert_eq!(
            state.selected_thread.as_ref(),
            Some(&id),
            "the first click asks about this thread"
        );
        assert_eq!(state.follow.as_ref(), Some(&id), "and binds the camera");

        rail_frame(&ctx, &snapshot, &mut state, click_at(at));
        assert_eq!(
            state.selected_thread, None,
            "the second click has to put the highlight away"
        );
        assert_eq!(state.follow, None, "and give the camera back");
        assert!(
            !state.interrogates(&id),
            "which is the whole point: the thread stops being the asked-about one"
        );
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
            ThreadStatus::Interrupted,
            ThreadStatus::Ready,
            ThreadStatus::Parked,
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
