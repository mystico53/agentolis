//! The terminal dock — the window's side of the session daemon (PRD §15 M7).
//!
//! `polis_term` knows about one pane: bytes in, a grid out, keys back. This
//! module knows about the *window*: where the dock sits, which pane is focused,
//! what the tab strip says, and how the daemon gets started in the first place.
//!
//! # Closing the window does not stop the agents
//!
//! [`Dock::shutdown`] drops a socket. That is the whole of it, and it is the
//! point of the architecture: the ptys live in `polis-sessiond`, so a closed
//! window, a crashed window or a GPU driver reset costs a *view*. Reopening
//! `polis work` reattaches, replays each pane's byte log through a fresh parser
//! and puts the same screens back — scrollback included.
//!
//! An agent is only ever stopped by an operator asking for it.
//!
//! # Focus arbitration needs no change to `read_keys`
//!
//! The instant a pane has focus, `ctx.egui_wants_keyboard_input()` is true —
//! verified at `egui-0.36.1/src/context.rs:2983`, which is exactly
//! `memory.focused().is_some()` — and [`crate::app`]'s `read_keys` already
//! returns an empty `Keys` in that case. So `t`, `i`, `h`, `s`, `r`, `p`, `n`,
//! `[`, `]`, `.`, `,`, Space and the arrows all go silent for the map with **no
//! change to the map's key handling at all**. Clicking the map surrenders focus
//! and they come back.
//!
//! What that does not give is a way to reach a map key without clicking, so
//! [`RESERVED`] is a small band of chords handled here, before the pane consumes
//! anything. `Ctrl+Alt` is used by neither Claude Code nor any shell.
//! **Not `Esc`** — Esc belongs to Claude Code, and stealing it would be the most
//! annoying possible choice.

// The dock is arithmetic against a pixel grid, like the map it sits beside.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use polis_events::SessionId;
use polis_term::client::{ClientEvent, PaneState, SessionClient};
use polis_term::emu::PaneSignal;
use polis_term::proto::{OpenPane, PaneId, Request};
use polis_term::transport::{self, Endpoint};
use polis_term::widget::{self, Metrics, PaintOptions};
use polis_term::{font, input};

use crate::palette;

pub use polis_term::input::interrupt_instead_of_copy;

/// The chords the dock keeps for itself, before a pane sees them.
///
/// Documented as data so the help sheet and the handler cannot drift apart.
pub const RESERVED: &[(&str, &str)] = &[
    ("Ctrl+Alt+1..9", "focus pane N"),
    ("Ctrl+Alt+M", "focus the map"),
    ("Ctrl+Alt+T", "new agent"),
    ("Ctrl+Alt+W", "close the focused pane"),
    ("Ctrl+`", "collapse or expand the dock"),
    ("F6", "cycle map → pane 1 → pane 2 → …"),
];

/// The point at which Claude Code's own layout stops working.
///
/// Its box-drawn input frame collapses below about sixty columns and its diffs
/// below eighty. The dock refuses to be narrower rather than showing a broken
/// TUI, and says so.
pub const MIN_COLS: u16 = 80;

/// Columns the dock opens at.
const DEFAULT_COLS: u16 = 100;

/// A dragged divider produces a resize per frame, and `ResizePseudoConsole`
/// makes the hosted app reflow its whole tree — so an undebounced drag makes
/// Claude Code strobe and burns real CPU inside the child.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(100);

/// How long the window waits for a daemon it just started to publish.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(10);

/// The font size a pane is drawn at.
const FONT_SIZE: f32 = 13.0;

/// Room for the panel's own margins around the grid.
const DOCK_PADDING: f32 = 16.0;

/// What the dock did this frame, for [`crate::app::Repaint`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DockFrame {
    /// A pane's screen changed, so the window is not idle.
    pub active: bool,
    /// A pane wants the operator's attention (a bell, or a child that exited).
    pub attention: bool,
}

/// One pane, as the window holds it.
struct Tab {
    state: Arc<PaneState>,
    /// The generation drawn last frame, so an unchanged pane is not a reason to
    /// draw another one.
    drawn: u64,
    /// The size last sent to the daemon, and when it changed.
    sent: (u16, u16),
    pending: Option<((u16, u16), Instant)>,
    /// Set by a bell or an exit; cleared when the tab is focused.
    attention: bool,
}

/// The terminal dock.
pub struct Dock {
    client: Option<SessionClient>,
    endpoint: Option<Endpoint>,
    tabs: BTreeMap<PaneId, Tab>,
    order: Vec<PaneId>,
    active: Option<PaneId>,
    metrics: Option<Metrics>,
    fonts: font::Installed,
    /// The repository panes are opened in.
    repo: PathBuf,
    /// What to run. `claude`, normally.
    program: String,
    args: Vec<String>,
    /// Extra environment for every pane — the telemetry block.
    env: Vec<(String, String)>,
    /// Collapsed to a chip rail.
    pub collapsed: bool,
    /// One sentence for the status bar; never a panic.
    pub status: String,
    /// How many panes have been opened, for session-id ordinals.
    opened: u32,
}

impl std::fmt::Debug for Dock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dock")
            .field("panes", &self.order.len())
            .field("active", &self.active)
            .field("collapsed", &self.collapsed)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl Dock {
    /// Connects to the session daemon, starting one if there is not one already.
    ///
    /// Never fails: a dock that cannot reach a daemon is a dock that says why in
    /// the status bar, because the map beside it is still worth looking at.
    pub fn start(
        ctx: &egui::Context,
        state_dir: Option<PathBuf>,
        repo: PathBuf,
        program: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> Self {
        let mut dock = Self {
            client: None,
            endpoint: None,
            tabs: BTreeMap::new(),
            order: Vec::new(),
            active: None,
            metrics: None,
            fonts: font::Installed::default(),
            repo,
            program,
            args,
            env,
            collapsed: false,
            status: String::new(),
            opened: 0,
        };
        let Some(dir) = state_dir.or_else(transport::default_state_dir) else {
            "no state directory, so no session daemon".clone_into(&mut dock.status);
            return dock;
        };
        match connect_or_start(&dir, ctx) {
            Ok((client, endpoint)) => {
                dock.status = format!(
                    "session daemon {} (pid {})",
                    client.daemon_version, endpoint.pid
                );
                dock.adopt_existing(&client);
                dock.client = Some(client);
                dock.endpoint = Some(endpoint);
            }
            Err(error) => dock.status = error,
        }
        dock
    }

    /// Installs the terminal font family. Call from the window's `theme`.
    pub fn install_fonts(&mut self, ctx: &egui::Context) {
        self.fonts = font::install(ctx);
    }

    /// The `polis doctor` line about glyph coverage.
    pub fn glyph_report(&self, ctx: &egui::Context) -> String {
        font::coverage_line(ctx, FONT_SIZE, &self.fonts)
    }

    /// Takes over every pane the daemon already had.
    ///
    /// This is reattach, and it is why the window can be closed: the panes were
    /// never the window's to begin with.
    fn adopt_existing(&mut self, client: &SessionClient) {
        let Ok(polis_term::proto::Reply::Panes { panes }) = client.call(Request::List) else {
            return;
        };
        for info in panes {
            let pane = info.pane;
            if let Ok(state) = client.adopt(info) {
                self.insert(pane, state);
            }
        }
        if !self.order.is_empty() {
            self.status = format!("reattached to {} pane(s)", self.order.len());
        }
    }

    fn insert(&mut self, pane: PaneId, state: Arc<PaneState>) {
        if self.tabs.contains_key(&pane) {
            return;
        }
        let sent = state
            .info
            .lock()
            .map_or((24, 80), |info| (info.rows, info.cols));
        self.tabs.insert(
            pane,
            Tab {
                state,
                drawn: u64::MAX,
                sent,
                pending: None,
                attention: false,
            },
        );
        self.order.push(pane);
        self.active.get_or_insert(pane);
    }

    /// True when there is a daemon to talk to.
    pub fn connected(&self) -> bool {
        self.client.as_ref().is_some_and(SessionClient::connected)
    }

    /// How many panes the dock is showing.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// True when no pane has been opened yet.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The session id of the pane an agent is running in, for the map.
    pub fn session_of(&self, pane: PaneId) -> Option<SessionId> {
        self.tabs
            .get(&pane)?
            .state
            .info
            .lock()
            .ok()?
            .session_id
            .clone()
    }

    /// The pane an agent's session is running in — the click that takes an
    /// operator from a cloud on the map to the terminal it belongs to.
    pub fn pane_for_session(&self, session: &SessionId) -> Option<PaneId> {
        self.order.iter().copied().find(|pane| {
            self.tabs
                .get(pane)
                .and_then(|tab| tab.state.info.lock().ok())
                .is_some_and(|info| info.session_id.as_ref() == Some(session))
        })
    }

    /// Starts an agent in a new pane.
    ///
    /// Polis issues the session id rather than inferring it: `claude
    /// --session-id <uuid>` is not gated on `--print`, and the same uuid then
    /// keys the OTLP resource attribute, the hook payload and the transcript's
    /// filename (ADR-0096).
    pub fn open(&mut self, rows: u16, cols: u16) {
        let Some(client) = self.client.as_ref() else {
            "no session daemon to open a pane in".clone_into(&mut self.status);
            return;
        };
        self.opened += 1;
        let session = polis_term::proto::fresh_session_id(self.opened);
        let mut args = self.args.clone();
        let names_claude = Path::new(&self.program)
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("claude"));
        if names_claude && !args.iter().any(|a| a == "--session-id") {
            args.push("--session-id".to_owned());
            args.push(session.as_str().to_owned());
        }
        let request = OpenPane {
            program: self.program.clone(),
            args,
            cwd: self.repo.clone(),
            env: pane_env(&self.env),
            rows: rows.max(1),
            cols: cols.max(MIN_COLS),
            session_id: names_claude.then_some(session),
        };
        match client.call(Request::Open(Box::new(request))) {
            Ok(polis_term::proto::Reply::Opened { info }) => {
                let pane = info.pane;
                if let Some(state) = client.pane(pane) {
                    self.insert(pane, state);
                } else if let Ok(state) = client.adopt(*info) {
                    self.insert(pane, state);
                }
                self.active = Some(pane);
                self.status = format!("opened {pane}");
            }
            Ok(other) => self.status = format!("the daemon answered open with {other:?}"),
            Err(error) => self.status = error,
        }
    }

    /// Drains everything the connection has to say. Call once per frame.
    pub fn poll(&mut self) -> DockFrame {
        let mut frame = DockFrame::default();
        let Some(events) = self
            .client
            .as_ref()
            .map(|client| client.events().try_iter().collect::<Vec<ClientEvent>>())
        else {
            return frame;
        };
        for event in events {
            match event {
                ClientEvent::Opened(info) => {
                    let pane = info.pane;
                    // Resolved before `insert`, so the borrow of `self.client`
                    // ends before `self` is borrowed mutably.
                    let state = self.client.as_ref().and_then(|client| client.pane(pane));
                    if let Some(state) = state {
                        self.insert(pane, state);
                    }
                }
                ClientEvent::Exited(pane, code) => {
                    if let Some(tab) = self.tabs.get_mut(&pane) {
                        tab.attention = true;
                    }
                    frame.attention = true;
                    self.status = match code {
                        Some(0) => format!("{pane} finished"),
                        Some(code) => format!("{pane} exited {code}"),
                        None => format!("{pane} ended"),
                    };
                }
                ClientEvent::Closed(pane) => {
                    self.tabs.remove(&pane);
                    self.order.retain(|p| *p != pane);
                    if self.active == Some(pane) {
                        self.active = self.order.first().copied();
                    }
                }
                ClientEvent::Signal(pane, PaneSignal::Bell) => {
                    if let Some(tab) = self.tabs.get_mut(&pane) {
                        tab.attention = true;
                    }
                    frame.attention = true;
                }
                ClientEvent::Signal(..) => {}
                ClientEvent::Disconnected(reason) => {
                    self.status = reason;
                }
            }
        }
        for pane in &self.order {
            if let Some(tab) = self.tabs.get(pane) {
                if tab.state.generation() != tab.drawn {
                    frame.active = true;
                }
            }
        }
        frame
    }

    /// The width the dock wants, in points.
    pub fn preferred_width(&self) -> f32 {
        self.metrics.as_ref().map_or(760.0, |metrics| {
            metrics.width_for(DEFAULT_COLS) + DOCK_PADDING
        })
    }

    /// The narrowest the dock may be before it stops being a terminal.
    ///
    /// [`MIN_COLS`] times the cell width, and this is the dock's one real cost:
    /// it sets a floor the map has to fit beside.
    pub fn minimum_width(&self) -> f32 {
        self.metrics
            .as_ref()
            .map_or(620.0, |metrics| metrics.width_for(MIN_COLS) + DOCK_PADDING)
    }

    /// Draws the dock's contents into `ui`.
    ///
    /// The caller owns the panel; this owns everything inside it.
    pub fn draw(&mut self, ui: &mut egui::Ui) -> DockFrame {
        let ctx = ui.ctx().clone();
        if self.metrics.is_none() {
            self.metrics = Some(Metrics::new(&ctx, FONT_SIZE));
        }

        if self.collapsed {
            self.draw_chips(ui);
            return DockFrame::default();
        }

        self.draw_tabs(ui);
        ui.separator();

        let Some(pane) = self.active else {
            self.draw_empty(ui);
            return DockFrame::default();
        };
        self.draw_pane(ui, pane)
    }

    /// The tab strip: one chip per pane, plus a new-agent button.
    fn draw_tabs(&mut self, ui: &mut egui::Ui) {
        let mut open_new = false;
        let mut close: Option<PaneId> = None;
        let mut select: Option<PaneId> = None;

        ui.horizontal_wrapped(|ui| {
            for (ordinal, pane) in self.order.iter().copied().enumerate() {
                let Some(tab) = self.tabs.get(&pane) else {
                    continue;
                };
                let info = tab.state.info.lock();
                let (title, alive, exit) = info.map_or_else(
                    |_| (String::from("?"), false, None),
                    |info| (info.title.clone(), info.alive(), info.exit),
                );
                let colour = if tab.attention {
                    palette::needs_decision().color()
                } else if alive {
                    palette::status(polis_world::ThreadStatus::Working).color()
                } else {
                    palette::status(polis_world::ThreadStatus::Idle).color()
                };
                let label = match exit {
                    Some(code) if code != 0 => format!("{} {title} · exit {code}", ordinal + 1),
                    Some(_) => format!("{} {title} · done", ordinal + 1),
                    None => format!("{} {title}", ordinal + 1),
                };
                let selected = self.active == Some(pane);
                if ui
                    .selectable_label(selected, RichText::new(label).color(colour).monospace())
                    .clicked()
                {
                    select = Some(pane);
                }
            }
            if ui.button("+ agent").on_hover_text("Ctrl+Alt+T").clicked() {
                open_new = true;
            }
            if let Some(pane) = self.active {
                if ui
                    .button("×")
                    .on_hover_text("Ctrl+Alt+W — end this agent")
                    .clicked()
                {
                    close = Some(pane);
                }
            }
        });

        if let Some(pane) = select {
            self.focus_tab(pane);
        }
        if let Some(pane) = close {
            self.close(pane);
        }
        if open_new {
            let (rows, cols) = self.last_size();
            self.open(rows, cols);
        }
    }

    /// Collapsed: a thin rail of one chip per pane, so a waiting agent stays
    /// visible with the dock shut.
    fn draw_chips(&mut self, ui: &mut egui::Ui) {
        let mut select = None;
        ui.vertical_centered(|ui| {
            for (ordinal, pane) in self.order.iter().copied().enumerate() {
                let Some(tab) = self.tabs.get(&pane) else {
                    continue;
                };
                let alive = tab.state.info.lock().is_ok_and(|info| info.alive());
                let colour = if tab.attention {
                    palette::needs_decision().color()
                } else if alive {
                    palette::status(polis_world::ThreadStatus::Working).color()
                } else {
                    palette::status(polis_world::ThreadStatus::Idle).color()
                };
                if ui
                    .selectable_label(
                        self.active == Some(pane),
                        RichText::new(format!("{}", ordinal + 1))
                            .monospace()
                            .color(colour),
                    )
                    .clicked()
                {
                    select = Some(pane);
                }
            }
        });
        if let Some(pane) = select {
            self.collapsed = false;
            self.focus_tab(pane);
        }
    }

    fn draw_empty(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            if self.connected() {
                ui.label(
                    RichText::new("no agents running here yet").color(palette::worker().color()),
                );
                ui.add_space(6.0);
                if ui.button("start one").clicked() {
                    let (rows, cols) = self.last_size();
                    self.open(rows, cols);
                }
            } else {
                ui.label(
                    RichText::new(&self.status)
                        .color(palette::contention().color())
                        .small(),
                );
            }
        });
    }

    /// The one pane drawn at full size.
    ///
    /// One function, because it is one ordered pass over a frame — measure,
    /// arbitrate focus, debounce a resize, read input, paint, then tell the
    /// daemon — and splitting it into six each called once would hide the order,
    /// which is the only thing about it that matters.
    ///
    /// Exactly one, ever: an unfocused pane is a tab label, not a rendered grid.
    /// That is both the largest single lever on the idle budget and the right
    /// shape for the product — the *map* is the parallelism view (PRD §11); you
    /// read one terminal.
    #[allow(clippy::too_many_lines)]
    fn draw_pane(&mut self, ui: &mut egui::Ui, pane: PaneId) -> DockFrame {
        let ctx = ui.ctx().clone();
        let metrics = self.metrics.as_ref().expect("built above");
        let rect = ui.available_rect_before_wrap();
        let (rows, cols) = metrics.grid_for(rect);

        if cols < MIN_COLS {
            ui.colored_label(
                palette::contention().color(),
                format!(
                    "too narrow — {cols} columns, and Claude Code's input frame needs \
                     {MIN_COLS}. Widen the window, or close the rail."
                ),
            );
            return DockFrame::default();
        }

        let id = egui::Id::new(("polis-pane", pane.0));
        let response = ui.interact(rect, id, egui::Sense::click_and_drag());
        if response.clicked() {
            response.request_focus();
        }
        let focused = response.has_focus();
        if focused {
            // Must be renewed every frame focus is held. These are the four keys
            // egui itself would otherwise steal for focus navigation, and all
            // four belong to the terminal.
            ui.memory_mut(|memory| {
                memory.set_focus_lock_filter(
                    id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                );
            });
        }

        let Some(tab) = self.tabs.get_mut(&pane) else {
            return DockFrame::default();
        };
        if focused {
            tab.attention = false;
        }

        // --- resize, debounced -------------------------------------------
        if (rows, cols) != tab.sent {
            match tab.pending {
                Some((size, _)) if size == (rows, cols) => {}
                _ => tab.pending = Some(((rows, cols), Instant::now())),
            }
        }
        let mut resize = None;
        if let Some((size, since)) = tab.pending {
            if size != (rows, cols) {
                tab.pending = Some(((rows, cols), Instant::now()));
            } else if since.elapsed() >= RESIZE_DEBOUNCE {
                tab.sent = size;
                tab.pending = None;
                resize = Some(size);
            }
        }

        // --- input --------------------------------------------------------
        let mode = tab
            .emulator_mode()
            .unwrap_or(alacritty_terminal::term::TermMode::empty());
        let mut keystrokes: Vec<u8> = Vec::new();
        let mut scroll = 0i32;
        if focused {
            ui.input(|state| {
                for event in &state.events {
                    if let Some(bytes) = input::to_bytes(event, mode) {
                        keystrokes.extend_from_slice(&bytes);
                    }
                }
                let lines = state.smooth_scroll_delta().y / metrics.cell.y;
                scroll = lines.round() as i32;
            });
        }
        if scroll != 0 {
            if let Ok(mut emulator) = tab.state.emulator.lock() {
                emulator.scroll(scroll);
            }
        }
        if !keystrokes.is_empty() {
            // Any key that produces bytes snaps back to the live bottom, which is
            // what every terminal does and what an operator who has scrolled up
            // and started typing means.
            if let Ok(mut emulator) = tab.state.emulator.lock() {
                emulator.scroll_to_bottom();
            }
        }

        // --- paint ---------------------------------------------------------
        let screen = tab
            .state
            .emulator
            .lock()
            .map(|emulator| emulator.snapshot())
            .ok();
        let mut frame = DockFrame::default();
        if let Some(screen) = screen {
            widget::paint(
                &ctx,
                ui.painter(),
                rect,
                &screen,
                metrics,
                PaintOptions {
                    focused,
                    background: Color32::from_rgb(10, 11, 14),
                    ..PaintOptions::default()
                },
            );
            frame.active = tab.drawn != screen.generation;
            tab.drawn = screen.generation;

            if screen.scrolled_back() {
                let chip = egui::Area::new(egui::Id::new(("polis-pane-scroll", pane.0)))
                    .order(egui::Order::Foreground)
                    .fixed_pos(rect.right_top() + egui::vec2(-220.0, 6.0));
                chip.show(&ctx, |ui| {
                    ui.label(
                        RichText::new(format!(
                            "scrolled back {} lines — press End",
                            screen.display_offset
                        ))
                        .small()
                        .color(palette::hover().color()),
                    );
                });
            }
        }
        frame.attention = tab.attention;

        // --- talk to the daemon ---------------------------------------------
        let client = self.client.as_ref();
        if let Some(client) = client {
            if let Some((rows, cols)) = resize {
                let _ = client.send(Request::Resize { pane, rows, cols });
            }
            if !keystrokes.is_empty() {
                let _ = client.send(Request::Write {
                    pane,
                    data: keystrokes,
                });
            }
        }
        frame
    }

    /// The reserved chords, handled before a pane consumes anything.
    ///
    /// Returns true when the frame's input was spent here.
    pub fn reserved_chords(&mut self, ctx: &egui::Context) -> bool {
        let mut handled = false;
        let chords: Vec<(egui::Key, egui::Modifiers)> = ctx.input(|state| {
            state
                .events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => Some((*key, *modifiers)),
                    _ => None,
                })
                .collect()
        });
        for (key, modifiers) in chords {
            let combo = modifiers.ctrl && modifiers.alt;
            match key {
                egui::Key::Backtick if modifiers.ctrl && !modifiers.alt => {
                    self.collapsed = !self.collapsed;
                    handled = true;
                }
                egui::Key::F6 => {
                    self.cycle(ctx);
                    handled = true;
                }
                egui::Key::T if combo => {
                    let (rows, cols) = self.last_size();
                    self.open(rows, cols);
                    handled = true;
                }
                egui::Key::W if combo => {
                    if let Some(pane) = self.active {
                        self.close(pane);
                    }
                    handled = true;
                }
                egui::Key::M if combo => {
                    // Focus is the whole of the arbitration: with none, egui
                    // stops wanting the keyboard and `read_keys` starts
                    // answering again.
                    ctx.memory_mut(egui::Memory::stop_text_input);
                    handled = true;
                }
                key if combo => {
                    if let Some(index) = digit(key) {
                        if let Some(pane) = self.order.get(index).copied() {
                            self.focus_tab(pane);
                            handled = true;
                        }
                    }
                }
                _ => {}
            }
        }
        handled
    }

    /// Map → pane 1 → pane 2 → … → map.
    fn cycle(&mut self, ctx: &egui::Context) {
        let focused = ctx.memory(egui::Memory::focused).is_some();
        if !focused {
            if let Some(pane) = self.order.first().copied() {
                self.focus_tab(pane);
            }
            return;
        }
        let current = self
            .active
            .and_then(|pane| self.order.iter().position(|p| *p == pane));
        match current {
            Some(index) if index + 1 < self.order.len() => {
                let next = self.order[index + 1];
                self.focus_tab(next);
            }
            _ => ctx.memory_mut(egui::Memory::stop_text_input),
        }
    }

    fn focus_tab(&mut self, pane: PaneId) {
        self.active = Some(pane);
        if let Some(tab) = self.tabs.get_mut(&pane) {
            tab.attention = false;
        }
    }

    /// Ends one pane's agent, for good.
    pub fn close(&mut self, pane: PaneId) {
        if let Some(client) = self.client.as_ref() {
            match client.call(Request::Close { pane }) {
                Ok(_) => self.status = format!("closed {pane}"),
                Err(error) => self.status = error,
            }
        }
        self.tabs.remove(&pane);
        self.order.retain(|p| *p != pane);
        if self.active == Some(pane) {
            self.active = self.order.first().copied();
        }
    }

    /// The size the last drawn pane had, for opening the next one.
    fn last_size(&self) -> (u16, u16) {
        self.active
            .and_then(|pane| self.tabs.get(&pane))
            .map_or((45, DEFAULT_COLS), |tab| tab.sent)
    }

    /// True when a pane has keyboard focus and no selection — the condition for
    /// rewriting `Event::Copy` back into Ctrl+C.
    pub fn wants_interrupt(&self, ctx: &egui::Context) -> bool {
        let Some(pane) = self.active else {
            return false;
        };
        let held = ctx.memory(egui::Memory::focused);
        if held != Some(egui::Id::new(("polis-pane", pane.0))) {
            return false;
        }
        self.tabs
            .get(&pane)
            .and_then(|tab| tab.state.emulator.lock().ok())
            .is_some_and(|emulator| emulator.snapshot().selection.is_none())
    }

    /// Lets go of the daemon. **The agents keep running.**
    ///
    /// This is the whole of the window's exit path for terminals, and its
    /// smallness is the point: `polis run`'s careful four-step child teardown is
    /// not needed here, because the window never owned a child.
    pub fn shutdown(&mut self) {
        self.tabs.clear();
        self.order.clear();
        self.client = None;
    }
}

/// The environment every pane gets on top of the daemon's own.
///
/// `TERM` and `COLORTERM` are here rather than in the daemon because they
/// describe *this* terminal: `ConPTY` sets neither, Ink reads `TERM` for its
/// colour detection, and without `COLORTERM` a diff degrades to sixteen colours.
///
/// **`TERM_PROGRAM` is deliberately unset.** Claude Code branches on it, and
/// claiming to be an unknown terminal is a worse bet than claiming nothing until
/// somebody measures which branch is better.
fn pane_env(extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut env = vec![
        ("TERM".to_owned(), "xterm-256color".to_owned()),
        ("COLORTERM".to_owned(), "truecolor".to_owned()),
    ];
    env.extend(extra.iter().cloned());
    env
}

/// `Ctrl+Alt+1..9` → an index into the tab order.
fn digit(key: egui::Key) -> Option<usize> {
    Some(match key {
        egui::Key::Num1 => 0,
        egui::Key::Num2 => 1,
        egui::Key::Num3 => 2,
        egui::Key::Num4 => 3,
        egui::Key::Num5 => 4,
        egui::Key::Num6 => 5,
        egui::Key::Num7 => 6,
        egui::Key::Num8 => 7,
        egui::Key::Num9 => 8,
        _ => return None,
    })
}

impl Tab {
    fn emulator_mode(&self) -> Option<alacritty_terminal::term::TermMode> {
        self.state.emulator.lock().ok().map(|e| e.mode())
    }
}

// ---------------------------------------------------------------------------
// Finding, or starting, the daemon
// ---------------------------------------------------------------------------

/// Connects to a running daemon, or starts one and connects to that.
fn connect_or_start(
    state_dir: &Path,
    ctx: &egui::Context,
) -> Result<(SessionClient, Endpoint), String> {
    let wake = {
        let ctx = ctx.clone();
        move || ctx.request_repaint()
    };
    if let Some(endpoint) = transport::discover(state_dir) {
        if let Ok(client) = SessionClient::connect(&endpoint, CLIENT_NAME, wake.clone()) {
            return Ok((client, endpoint));
        }
        // A stale file. `spawn_daemon` takes it over.
    }
    spawn_daemon(state_dir)?;
    let deadline = Instant::now() + DAEMON_START_TIMEOUT;
    let mut last = "the session daemon did not publish an endpoint".to_owned();
    while Instant::now() < deadline {
        if let Some(endpoint) = transport::discover(state_dir) {
            match SessionClient::connect(&endpoint, CLIENT_NAME, wake.clone()) {
                Ok(client) => return Ok((client, endpoint)),
                Err(error) => last = error,
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(last)
}

/// What the daemon's log calls us.
const CLIENT_NAME: &str = concat!("polis-app ", env!("CARGO_PKG_VERSION"));

/// Starts `polis-sessiond`, detached, so it survives this window.
fn spawn_daemon(state_dir: &Path) -> Result<(), String> {
    let binary = daemon_binary().ok_or_else(|| {
        "polis-sessiond is not beside polis or on PATH — build it with \
         `cargo build -p polis-sessiond`"
            .to_owned()
    })?;
    let mut command = std::process::Command::new(&binary);
    command
        .arg("--state-dir")
        .arg(state_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP. Without the first, a
        // daemon started from a console inherits it and dies with the terminal
        // that launched Polis; without the second, a Ctrl+C in that terminal
        // reaches every agent it is holding.
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not start {}: {error}", binary.display()))
}

/// Where the daemon binary is: beside this executable, then on `PATH`.
fn daemon_binary() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "polis-sessiond.exe"
    } else {
        "polis-sessiond"
    };
    if let Some(beside) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
    {
        if beside.is_file() {
            return Some(beside);
        }
    }
    polis_term::exec::which("polis-sessiond")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chords are documented as data so the help sheet cannot drift from
    /// the handler. Both halves have to name the same set.
    #[test]
    fn every_reserved_chord_is_documented() {
        assert!(!RESERVED.is_empty());
        for (chord, what) in RESERVED {
            assert!(!chord.is_empty());
            assert!(!what.is_empty(), "{chord} needs a description");
        }
        let text: String = RESERVED
            .iter()
            .map(|(c, _)| *c)
            .collect::<Vec<_>>()
            .join(" ");
        for expect in ["Ctrl+Alt+T", "Ctrl+Alt+W", "F6"] {
            assert!(
                text.contains(expect),
                "{expect} is handled but undocumented"
            );
        }
        assert!(
            !text.contains("Esc"),
            "Esc belongs to Claude Code — interrupt and clear-input both use it"
        );
    }

    #[test]
    fn the_digit_chords_map_to_tab_positions() {
        assert_eq!(digit(egui::Key::Num1), Some(0));
        assert_eq!(digit(egui::Key::Num9), Some(8));
        assert_eq!(digit(egui::Key::A), None);
    }

    /// `TERM` and `COLORTERM` are what `ConPTY` does not set and Ink reads;
    /// `TERM_PROGRAM` is deliberately absent.
    #[test]
    fn a_pane_gets_the_two_variables_conpty_does_not_set() {
        let env = pane_env(&[("OTEL_SERVICE_NAME".to_owned(), "claude".to_owned())]);
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"TERM"));
        assert!(names.contains(&"COLORTERM"));
        assert!(
            names.contains(&"OTEL_SERVICE_NAME"),
            "the caller's block survives"
        );
        assert!(
            !names.contains(&"TERM_PROGRAM"),
            "claiming to be an unknown terminal is a worse bet than claiming nothing"
        );
    }

    /// Eighty columns is a hard requirement of Claude Code's own layout, so the
    /// floor has to be expressed in cells and not in pixels.
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn the_column_floor_is_a_real_number_of_columns() {
        // Constant on purpose: these are the two numbers Claude Code's own
        // layout imposes, and the test exists so that lowering either one has
        // to be a deliberate edit to a line that says why.
        assert!(MIN_COLS >= 80, "Claude Code's diffs collapse below 80");
        assert!(
            DEFAULT_COLS > MIN_COLS,
            "opening at the floor leaves no room"
        );
    }
}
