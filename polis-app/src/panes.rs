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
    /// What the *map* knows about this pane's agent, refreshed every frame by
    /// [`Dock::observe`] from the world snapshot.
    ///
    /// `None` means the correlation has not landed yet — the agent has started
    /// but no channel has carried its session id onto the bus. That is a
    /// legitimate state for the first second or two of a pane's life, and it is
    /// drawn as an absence rather than as a claim.
    seen: Option<Seen>,
}

/// One pane's agent, as the city sees it.
///
/// The whole of the map-to-dock direction: a tab that can say where its agent
/// is working and whether it is waiting on a human is a tab the operator can
/// read without looking at the map at all.
#[derive(Debug, Clone, Default)]
struct Seen {
    /// PRD §6.2's claim, or §6.4's heaviest lobe — whatever the rail would say.
    place: Option<String>,
    /// Working / Waiting / Done / …, derived by `polis-world`, never authored.
    status: Option<polis_world::ThreadStatus>,
    /// An attention mark points at this agent and it is the amber one.
    needs_decision: bool,
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
    /// Which agent the pane is currently pointed at, from the agent list.
    ///
    /// Kept so the empty state can tell the two silences apart: nothing
    /// selected, and an agent selected that is not running in this dock.
    showing: Option<polis_events::ThreadId>,
    /// The pane the operator moved to this frame, taken by [`Dock::take_reveal`].
    ///
    /// Recorded rather than acted on, because the map is drawn after the dock
    /// and the selection it publishes belongs to the frame it was clicked in.
    revealed: Option<PaneId>,
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
            showing: None,
            revealed: None,
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

    /// Takes the terminal font family the window bound at start-up.
    ///
    /// The dock does **not** install it. `Context::set_fonts` takes effect on
    /// the *next* frame, and a dock created part-way through a frame draws its
    /// pane later in that same one — so installing here panicked epaint with
    /// `FontFamily::Name("polis-term") is not bound to any fonts` the moment
    /// the dock stopped being built before the first frame. See
    /// [`crate::app::PolisApp::new`], which binds it unconditionally.
    pub fn use_fonts(&mut self, installed: font::Installed) {
        self.fonts = installed;
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
                seen: None,
            },
        );
        self.order.push(pane);
        if self.active.is_none() {
            self.active = Some(pane);
            self.revealed = Some(pane);
        }
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

    /// Points the pane at whichever agent the list has selected.
    ///
    /// The dock used to answer *which agent am I looking at* with its own tab
    /// strip. It now answers it with the **selection**, which is the same
    /// question the map and the agent list already answer, so the three cannot
    /// disagree and there is no second selector to keep in sync.
    ///
    /// Three cases, and the middle one is the one worth naming:
    ///
    /// * nothing selected — the pane shows its empty state;
    /// * an agent selected that is **not** running here — Polis is only
    ///   *watching* it, which is the ordinary case for a session started in
    ///   some other terminal. The pane says so. It must not fall back to
    ///   whatever was on screen before, because a terminal showing a different
    ///   agent than the one the operator just clicked is worse than a terminal
    ///   showing nothing;
    /// * an agent selected that is running here — its screen.
    pub fn show_selected(&mut self, thread: Option<&polis_events::ThreadId>) {
        self.showing = thread.cloned();
        let Some(thread) = thread else {
            // # Nothing selected keeps the pane, it does not blank it
            //
            // Blanking here was wrong twice. Clicking `+ agent` starts an agent
            // and selects nothing — so the pane the operator had just asked for
            // was hidden behind *"select an agent to see its terminal"* until
            // the map noticed it, which needs a tool call and a channel. And
            // `polis work --panes 1` opened its window on the same message with
            // a live agent one pane away.
            //
            // So an empty selection means "no opinion", not "show nothing": the
            // front pane stays, and the empty state is reached the only way it
            // should be — by there being no panes.
            if self
                .active
                .is_none_or(|pane| !self.tabs.contains_key(&pane))
            {
                self.active = self.order.first().copied();
            }
            return;
        };
        // `focus_tab` rather than a bare assignment: it clears the tab's bell
        // and records the reveal, so selecting an agent here and selecting it on
        // the map end in exactly the same state.
        if let Some(pane) = self.pane_for_session(thread.session()) {
            if self.active != Some(pane) {
                self.focus_tab(pane);
            }
        } else {
            self.active = None;
        }
    }

    /// Brings an agent's pane to the front, opening the dock if it was shut.
    ///
    /// This is the map-to-dock half of PRD §15 M7d: a cloud goes amber, the
    /// operator picks that agent — its rail row, or `a`, which jumps to
    /// whatever is waiting on a human — and the terminal it is typing into is
    /// the one on screen. The map's own anchors are hover targets and not yet
    /// click targets, so an anchor is the one route this does not have. Returns false when no pane is running that session —
    /// which is the ordinary case for an agent Polis is only *watching*, so it
    /// is not an error and nothing is said about it.
    pub fn focus_session(&mut self, session: &SessionId) -> bool {
        let Some(pane) = self.pane_for_session(session) else {
            return false;
        };
        if self.active == Some(pane) && !self.collapsed {
            return true;
        }
        self.collapsed = false;
        self.focus_tab(pane);
        true
    }

    /// The pane the operator moved to since this was last called, if any.
    ///
    /// Taken rather than read, because acting on it twice would fight the
    /// operator for the map's selection every frame.
    pub fn take_reveal(&mut self) -> Option<SessionId> {
        let pane = self.revealed.take()?;
        self.session_of(pane)
    }

    /// Tells the dock what the city knows about each of its agents.
    ///
    /// Call once a frame, after the world has been published. Everything it
    /// writes is derived — the dock is a *reader* of the world here, exactly as
    /// the map and the rail are, so a tab and a cloud can never disagree about
    /// the same agent.
    ///
    /// The amber flag is deliberately **not** merged into [`Tab::attention`]'s
    /// sticky bell flag: a bell is a thing that happened and stays until it is
    /// looked at, while *waiting on you* is a thing that is **true right now**
    /// and has to stop being true the instant the agent is unblocked — even if
    /// the operator answered the prompt in the pane without ever clicking the
    /// tab.
    pub fn observe(&mut self, snapshot: &polis_world::snapshot::WorldSnapshot) {
        use polis_world::attention::AttentionKind;

        // Walked over the tabs themselves rather than over `order`, and the
        // session id read straight off the pane: this runs every frame, and the
        // obvious spelling — `for pane in self.order.clone()` around
        // `self.session_of` — buys a `Vec` per frame to work around a borrow
        // that is not actually there. Both scans below are over a handful of
        // panes and a capped mark list, so neither allocates either.
        for tab in self.tabs.values_mut() {
            let session = tab
                .state
                .info
                .lock()
                .ok()
                .and_then(|info| info.session_id.clone());
            // No session id means a pane running something that is not Claude
            // Code, or one whose id has not reached the bus yet. Cleared rather
            // than left standing: a stale district is worse than none.
            let Some(session) = session else {
                tab.seen = None;
                continue;
            };
            tab.seen = snapshot
                .threads
                .iter()
                .find(|thread| thread.session_id == session)
                .map(|thread| Seen {
                    place: place_label(&thread.territory),
                    status: Some(thread.status),
                    needs_decision: snapshot.attention.iter().any(|mark| {
                        matches!(mark.kind, AttentionKind::NeedsDecision { .. })
                            && *mark.thread().session() == session
                    }),
                });
        }
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
                self.revealed = Some(pane);
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
                    // # A clean exit takes its tab with it
                    //
                    // The daemon keeps an exited pane on purpose — its screen
                    // and its code survive so a reattached window can still read
                    // how it ended (`polis_sessiond::panes`, and the test that
                    // pins it). The *window* had no matching rule, so a pane the
                    // operator had just quit sat in the strip for ever reading
                    // `done`, over an empty grid, because Claude Code clears the
                    // screen on its way out. Two dead things and no way to tell
                    // they were finished rather than broken.
                    //
                    // `Some(0)` is the operator's own `/exit`: they know how it
                    // ended, there is nothing left to read, and the tab is
                    // clutter. Anything else is **news** — a non-zero code or a
                    // child that was killed — so that pane stays, with its last
                    // screen and a banner saying what happened.
                    if closes_itself(code) {
                        self.close(pane);
                        self.status = format!("{pane} finished");
                    } else {
                        if let Some(tab) = self.tabs.get_mut(&pane) {
                            tab.attention = true;
                        }
                        frame.attention = true;
                        self.status = match code {
                            Some(code) => format!("{pane} exited {code}"),
                            None => format!("{pane} ended"),
                        };
                    }
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
                let seen = tab.seen.clone().unwrap_or_default();
                // The map's colour wins when the map has one, so a tab and its
                // cloud say the same thing about the same agent. `alive` is the
                // pty's answer and only stands in until a channel has spoken.
                let colour = if tab.attention || seen.needs_decision {
                    palette::needs_decision().color()
                } else if let Some(status) = seen.status {
                    palette::status(status).color()
                } else if alive {
                    palette::status(polis_world::ThreadStatus::Working).color()
                } else {
                    palette::status(polis_world::ThreadStatus::Idle).color()
                };
                let label = match exit {
                    Some(code) if code != 0 => format!("{} {title} · exit {code}", ordinal + 1),
                    Some(_) => format!("{} {title} · done", ordinal + 1),
                    None => match &seen.place {
                        // Where the agent is working is worth more of a narrow
                        // tab strip than a repeated program name is.
                        Some(place) => format!("{} {}", ordinal + 1, tail(place)),
                        None => format!("{} {title}", ordinal + 1),
                    },
                };
                let selected = self.active == Some(pane);
                let chip =
                    ui.selectable_label(selected, RichText::new(label).color(colour).monospace());
                let chip = match (&seen.place, seen.status) {
                    // `status_word`, not `{status:?}`: the rail and the map say
                    // this word about this thread, and a tab inventing its own
                    // spelling is exactly the disagreement that vocabulary
                    // exists to prevent.
                    (Some(place), Some(status)) => chip.on_hover_text(format!(
                        "{title} — {} in {place}\nthis agent's cloud is on the map",
                        crate::ui::status_word(status).trim()
                    )),
                    _ if alive => chip.on_hover_text(format!(
                        "{title} — running, but no channel has carried its session id \
                         onto the map yet"
                    )),
                    _ => chip,
                };
                if chip.clicked() {
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
                let seen = tab.seen.clone().unwrap_or_default();
                let colour = if tab.attention || seen.needs_decision {
                    palette::needs_decision().color()
                } else if let Some(status) = seen.status {
                    palette::status(status).color()
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

    /// What the pane shows when it is not showing a terminal.
    ///
    /// Four different silences, and saying "no agents" for all four is how an
    /// operator learns to stop reading this column. In order of how often they
    /// happen: nothing selected, an agent selected that Polis is only watching,
    /// nothing running here at all, and no daemon to ask.
    fn draw_empty(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            if !self.connected() {
                ui.label(
                    RichText::new(&self.status)
                        .color(palette::contention().color())
                        .small(),
                );
                return;
            }
            if self.showing.is_some() && !self.order.is_empty() {
                // The selected agent is real and on the map; it is simply not
                // one of ours. Saying which is the difference between a window
                // that looks broken and one that is telling the truth: most
                // agents on a `polis watch` map were started in some other
                // terminal, and Polis cannot attach to a pty it did not create.
                ui.label(
                    RichText::new("this agent is running somewhere else")
                        .color(palette::worker().color()),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Polis is watching it, not hosting it — only a session started \
                         here has a terminal to show.",
                    )
                    .small()
                    .color(palette::worker().color())
                    .line_height(Some(15.0)),
                );
            } else if self.order.is_empty() {
                ui.label(
                    RichText::new("no agents running here yet").color(palette::worker().color()),
                );
            } else {
                ui.label(
                    RichText::new("select an agent to see its terminal")
                        .color(palette::worker().color()),
                );
            }
            ui.add_space(8.0);
            if ui
                .button("+ agent")
                .on_hover_text("Ctrl+Alt+T — start a Claude Code session in this checkout")
                .clicked()
            {
                let (rows, cols) = self.last_size();
                self.open(rows, cols);
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
        let mut rect = ui.available_rect_before_wrap();
        let (mut rows, mut cols) = metrics.grid_for(rect);

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

        // A pane only reaches here still exited when it ended badly — a clean
        // exit closed itself above. Its last screen is the evidence, so the
        // banner goes *over* it rather than replacing it, and the way out is on
        // the banner rather than only in a chord nobody has memorised.
        let ended = tab_exit(self.tabs.get(&pane));
        if let Some(how) = ended {
            let mut close = false;
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(match how {
                        Ended::Code(code) => format!("this agent exited {code}"),
                        Ended::Killed => "this agent was ended".to_owned(),
                    })
                    .color(palette::contention().color()),
                );
                close = ui
                    .button("close pane")
                    .on_hover_text("Ctrl+Alt+W — the screen below is its last")
                    .clicked();
            });
            ui.separator();
            if close {
                self.close(pane);
                return DockFrame::default();
            }
        }

        if ended.is_some() {
            rect = ui.available_rect_before_wrap();
            let grid = metrics.grid_for(rect);
            rows = grid.0;
            cols = grid.1;
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
        let moved = self.active != Some(pane);
        self.active = Some(pane);
        if moved {
            self.revealed = Some(pane);
        }
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
/// What a tab calls the place its agent is working, or `None` for nowhere yet.
///
/// The rail's `place_of` answers the same question with a whole paragraph and a
/// hover; a tab has room for about twenty characters, so a claim is written
/// plainly and lobes are written as the heaviest one and a count. What it must
/// **not** do is invent a place for a thread PRD §6.2 has refused to place —
/// `unplaced` is a real state, and a tab that guessed would be the map and the
/// dock disagreeing about the same agent.
fn place_label(territory: &polis_world::territory::Territory) -> Option<String> {
    use polis_world::territory::Placement;

    match territory.placement() {
        Placement::Claim(claim) => Some(claim.as_str().to_owned()),
        Placement::Lobes(lobes) => lobes.first().map(|lobe| match lobes.len() {
            1 => lobe.path.as_str().to_owned(),
            n => format!("{} +{}", lobe.path.as_str(), n - 1),
        }),
        Placement::Nowhere => None,
    }
}

/// The last two segments of a place, for a tab that has no room for the rest.
///
/// The tab strip is `horizontal_wrapped` inside a dock with an eighty-column
/// floor, so a full `polis-app/src/components/widgets` on three tabs costs the
/// terminal a row of its own height for something the operator can already see
/// on the map. Two segments is what PRD §6.2's claims mostly are —
/// `MIN_CLAIM_DEPTH` is 2 — so for a lobe this trims nothing at all. Applied
/// **here and not in [`place_label`]**, because the hover shows the whole path
/// and a truncation that reached the data would have thrown it away.
fn tail(place: &str) -> &str {
    match place.rmatch_indices('/').nth(1) {
        Some((cut, _)) => &place[cut + 1..],
        None => place,
    }
}

/// Whether a pane that has just exited should take its tab with it.
///
/// `Some(0)` is the operator's own `/exit`: they know how it ended, the screen
/// Claude Code left behind is blank because it clears on the way out, and the
/// tab is clutter that reads `done` for ever. Everything else is news and is
/// kept — a non-zero code, and `None` for a child that was killed or lost with
/// its daemon, which is the case a rule written as `code != Some(0)` would have
/// got right by accident and a rule written as `code.is_some()` would have got
/// wrong.
fn closes_itself(code: Option<i32>) -> bool {
    code == Some(0)
}

/// How a pane's child ended, or `None` while it is still running.
///
/// `Some(None)` is a child that was ended without a code — killed, or the daemon
/// taken down under it — which is a different sentence from `Some(Some(3))` and
/// has to stay distinguishable.
fn tab_exit(tab: Option<&Tab>) -> Option<Ended> {
    let info = tab?.state.info.lock().ok()?;
    if info.alive() {
        return None;
    }
    Some(info.exit.map_or(Ended::Killed, Ended::Code))
}

/// How a pane's child ended, once it has.
///
/// A named pair rather than `Option<Option<i32>>`, which is the same two cases
/// spelled so that neither the compiler nor a reader can tell which `None` is
/// which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// It exited, with this code.
    Code(i32),
    /// It ended without one — killed, or lost with its daemon.
    Killed,
}

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

    /// PRD §6.2's three placements, as a tab writes them.
    #[test]
    fn a_tab_says_where_its_agent_is_and_never_guesses() {
        use polis_world::territory::{Lobe, Territory};

        let path = |s: &str| polis_events::LogicalPath::new(s).expect("a valid logical path");
        let mut territory = Territory::default();
        // Nowhere: no claim, no lobes. A thread the world has refused to place
        // reads as an absence on the tab, exactly as it reads on the map.
        assert_eq!(place_label(&territory), None);

        // §6.4's lobes, heaviest first, with the rest counted rather than named.
        territory.lobes = vec![
            Lobe {
                path: path("src/components"),
                mass: 0.6,
            },
            Lobe {
                path: path("src/hooks"),
                mass: 0.4,
            },
        ];
        assert_eq!(
            place_label(&territory).as_deref(),
            Some("src/components +1")
        );

        // A converged claim wins over the lobes that produced it.
        territory.claim = Some(path("src/services"));
        assert_eq!(place_label(&territory).as_deref(), Some("src/services"));

        // The data keeps the whole path; only the tab shortens it.
        territory.claim = Some(path("polis-app/src/components/widgets"));
        assert_eq!(
            place_label(&territory).as_deref(),
            Some("polis-app/src/components/widgets")
        );
    }

    /// A tab has about twenty characters. The map has the rest.
    #[test]
    fn a_tab_shortens_a_deep_place_and_leaves_a_shallow_one_alone() {
        assert_eq!(
            tail("polis-app/src/components/widgets"),
            "components/widgets"
        );
        assert_eq!(tail("src/services"), "src/services");
        assert_eq!(tail("src"), "src");
        assert_eq!(tail(""), "");
        // The lobe form keeps its count, because the count is the part that
        // says this thread is in more than one place.
        assert_eq!(tail("a/b/c +2"), "b/c +2");
    }

    /// > when i close a claude code instance it just says "done" but the
    /// > terminal tab doesn't close, or no terminal shows
    ///
    /// Both halves were one omission: the daemon keeps an exited pane on purpose
    /// and the window had no matching rule, so the tab stayed for ever over the
    /// blank screen Claude Code leaves when it clears on exit.
    #[test]
    fn a_clean_exit_closes_its_own_pane_and_a_failure_stays() {
        assert!(closes_itself(Some(0)), "the operator's own /exit");
        assert!(!closes_itself(Some(1)), "a non-zero code is news");
        assert!(!closes_itself(Some(130)), "an interrupt is news");
        // Killed, or lost with its daemon. No code at all is not success, and
        // this is the case that decides between `code != Some(0)` and
        // `code.is_some()` — the second would throw the evidence away.
        assert!(!closes_itself(None), "a child that was ended is news");
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
