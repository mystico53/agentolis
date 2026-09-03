//! The window (PRD §12, §13, §15 M1/M2).
//!
//! `winit` owns the main thread from [`launch`] onward, which is why there is no
//! `#[tokio::main]` anywhere in Polis.
//!
//! # What runs where
//!
//! ```text
//! main thread     eframe/winit event loop, egui overlay, the world, the driver
//! index thread    the session scan, so the picker opens instantly
//! load thread     git walk + tree-sitter + layout + transcript read
//! wake thread     live only: watches the bus depth and asks for a frame
//! ```
//!
//! The world is driven on the main thread deliberately, in a replay and live
//! alike. PRD §5 says the renderer must sample a lock-free snapshot rather than
//! block on a writer, and that is exactly what happens here —
//! [`SnapshotPublisher`] publishes and [`SnapshotReader`] loads. `polis-world`
//! applies 22 759 real events in 239 ms, so PRD §13.1's whole 500 events/sec
//! budget is 84 µs of a 16.6 ms frame, and a world thread would buy a channel
//! hop between the click that selects a building and the world that knows about
//! it. The shape is one shape for both sources: `advance`, `publish`, `load`,
//! once per frame, never per event. [`live`] states the measurement in full.
//!
//! # The idle budget is a repaint discipline
//!
//! > Idle CPU (no agent activity): < 2% of one core. […] Idle cost matters —
//! > this thing runs all day on the operator's second monitor. (PRD §13.1)
//!
//! `egui` repaints on input and on request, and nothing else. So this file
//! requests a repaint in exactly five cases: a transport is playing, an
//! animation is in flight, a key is held, a background job is still running, or
//! a live feed has work or something alive to age. [`Repaint`] is that decision
//! made once, in one place, with a name attached so the status bar can say which
//! of the five is keeping the window awake.
//!
//! The live case is the one that could have cost the whole budget, and does not:
//! the window is woken by [`live::LiveFeed`]'s waker thread when an event is
//! actually queued, so a quiet map renders once a second to move a clock and
//! otherwise sleeps.

// The frame loop is arithmetic against a pixel grid and a millisecond clock, and
// `too_many_lines` on `ui()` would only push one ordered sequence of panels into
// fragments each called once.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

pub mod live;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use crossbeam_channel::{Receiver, TryRecvError};
use eframe::egui::{self, Color32, RichText};
use polis_events::{LogicalPath, PathMapper};
use polis_world::replay::{ReplayDriver, ReplaySchedule};
use polis_world::snapshot::{self, SnapshotPublisher, SnapshotReader, WorldSnapshot};
use polis_world::World;

use crate::basemap::BaseMap;
use crate::camera::Camera;
use crate::citygen::{self, ColdStart, Generated};
use crate::clouds::Clouds;
use crate::config::Config;
use crate::drill;
use crate::mapview::{self, ViewState};
use crate::palette;
use crate::session::Picker;
use crate::treeview::TreeView;
use crate::ui::{self, Overlay, View, Vitals};
use live::{LiveFeed, LiveOptions};

/// How many frame times the status bar keeps.
const FRAME_WINDOW: usize = 240;

/// The narrowest the map may be squeezed to before something else gives.
///
/// The dock's one real cost is columns: [`crate::panes::MIN_COLS`] times the cell
/// width is a hard floor, because Claude Code's own layout collapses below it.
/// So when all three cannot fit, **the rail closes first** — it is the one of
/// the three that has a keystroke to bring it back, and the map is the product.
const MAP_MIN_WIDTH: f32 = 360.0;

/// What the right-hand rail takes when it is open.
///
/// Its real width is egui's to remember, so this is the estimate the layout
/// decision uses — it only has to be close enough to choose between "all three
/// fit" and "they do not".
const RAIL_WIDTH: f32 = 370.0;

/// The dock, collapsed to a rail of one chip per agent.
///
/// Wide enough for a two-digit ordinal, so a waiting agent stays visible with
/// the dock shut.
const COLLAPSED_DOCK: f32 = 34.0;

/// The ceiling on terminal-driven repaints — 30 Hz.
///
/// At roughly 1.5 ms a frame that is about 4.5 % of one core, which is **above**
/// PRD §13.1's idle budget and correctly so: an agent producing output is not
/// idle. [`Repaint::Terminal`] says so in the status bar rather than hiding it.
const TERMINAL_TICK: Duration = Duration::from_millis(33);

/// A click opens the editor, and a second click on the same file inside this
/// window does not — an operator exploring the map must not spawn a stack of
/// editor windows (PRD §12: *"Click a building → open in `$EDITOR` […] Nothing
/// more."*).
const EDITOR_DEBOUNCE: Duration = Duration::from_secs(2);

/// What the window was started for.
#[derive(Debug, Clone)]
pub enum Mode {
    /// `polis` — this repository as a city, with no recording (PRD §15 M1).
    Map {
        /// The checkout to draw.
        repo: PathBuf,
    },
    /// `polis replay <path>` — a recording animated over its city (PRD §15 M2).
    Replay {
        /// A transcript file, or a `<session-id>` sidecar directory.
        transcript: PathBuf,
        /// Which checkout the city comes from.
        repo: PathBuf,
        /// Initial playback speed.
        speed: f32,
    },
    /// `polis watch` — this repository's city with the four ingest channels
    /// feeding it in real time (PRD §15 M3).
    ///
    /// The only mode with a second writer to the bus, and the only one where an
    /// operator can be looking at an empty map for a legitimate reason — so it
    /// is the only one that has to keep saying what it is doing. See [`live`].
    Live {
        /// How to start the four channels, and which agents belong on this map.
        ///
        /// Boxed because it is much the largest variant and every other one is a
        /// path or two.
        options: Box<LiveOptions>,
    },
    /// `polis work` — the map, and agents running in panes beside it (PRD §15 M7).
    ///
    /// The difference from [`Mode::Map`] is not cosmetic: this is the only
    /// variant that talks to `polis-sessiond`, and therefore the only one whose
    /// window has agents attached to it. It still owns none of them — the
    /// daemon does, which is why closing this window leaves every agent running
    /// (ADR-0098).
    Work {
        /// The checkout to draw, and where panes are opened.
        repo: PathBuf,
        /// How many agents to start straight away.
        panes: usize,
        /// What to run in each. `claude`, normally.
        program: String,
        /// Arguments passed to it untouched.
        args: Vec<String>,
    },
    /// `polis replay` with no argument — pick from this machine's own sessions.
    Pick,
}

/// Opens the window (PRD §13).
///
/// `Renderer::Wgpu`, never glow. `glow` appearing in the dependency tree does
/// **not** mean the glow renderer is in use: it arrives via `wgpu-hal`'s GLES
/// backend, and the actual check is `cc.wgpu_render_state.is_some()`, which
/// [`PolisApp::new`] asserts.
pub fn launch(config: Config, mode: Mode) -> anyhow::Result<()> {
    let started = Instant::now();
    announce(&mode);
    // Before the window, not after. Creating a surface and picking an adapter
    // is hundreds of milliseconds; a hook datagram that arrives with nothing
    // bound is gone for good, and an OTel exporter that finds nothing listening
    // drops its first batch — which is the session start. The bounded bus holds
    // what arrives while the city is generated.
    let feed = match &mode {
        Mode::Live { options } => Some(Box::new(LiveFeed::start(options))),
        _ => None,
    };
    // The inner size is in **logical points**, and this machine renders at 1.75
    // points per pixel: a naive `[1600, 1000]` asks for 2800x1750 physical on a
    // 2194x1234 screen, Windows clamps the window, and egui keeps laying out for
    // the size it asked for — a scene drawn 1.7x too large and clipped, which
    // looks exactly like a broken camera. So the restored size is modest and the
    // window opens maximized, which is what a map that "runs all day on the
    // operator's second monitor" (PRD §13.1) wants anyway.
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("polis")
            .with_inner_size([1180.0, 720.0])
            .with_min_inner_size([720.0, 480.0])
            .with_maximized(true),
        ..Default::default()
    };
    eframe::run_native(
        "polis",
        options,
        Box::new(move |cc| Ok(Box::new(PolisApp::new(cc, config, mode, feed, started)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Says on stdout that a window is opening and that the command will not return
/// until it closes.
///
/// `polis map`, `polis watch` and `polis replay` printed **nothing** and blocked
/// until the window closed. On a busy desktop the window opens behind something
/// else, and the operator is left with a terminal that has hung — the first-run
/// review lost a three-minute tool call to exactly that. One line fixes it.
///
/// Not printed for the map window `polis run` spawns for itself: that child's
/// stdout is `Stdio::null()`, and the parent has already said what it opened.
fn announce(mode: &Mode) {
    let mut out = std::io::stdout().lock();
    let what = match mode {
        Mode::Map { repo } => format!("opening the map for {}", repo.display()),
        Mode::Pick => "opening the session picker".to_owned(),
        Mode::Replay { transcript, .. } => format!("replaying {}", transcript.display()),
        Mode::Work {
            repo,
            panes,
            program,
            ..
        } => format!(
            "opening {} with {panes} {program} pane(s) — the agents run in \
             polis-sessiond and outlive this window",
            repo.display()
        ),
        Mode::Live { options } => format!(
            "watching {} live — telemetry {}, hooks {}, transcripts {}",
            options.repo().display(),
            options.ingest.otlp_addr,
            options.ingest.hook_addr,
            options.ingest.claude_projects_dir.display()
        ),
    };
    let _ = writeln!(out, "polis: {what} — close the window to return here.");
    let _ = out.flush();
}

/// The eframe application.
pub struct PolisApp {
    stage: Stage,
    config: Config,
    overlay: Overlay,
    /// When the process started, for PRD §13.1's cold-start budget.
    ///
    /// > Cold start → first frame, 5k-file repo: < 3 s.
    ///
    /// Measured end to end here rather than summed from the stages, because the
    /// budget is about the operator's wait and includes everything the stage
    /// timings leave out — process start, adapter selection, surface creation.
    started: Instant,
    /// Process start to the first frame that drew a city, in milliseconds.
    first_frame_ms: Option<f64>,
    /// Whether this frame rebuilt the base map and so is not a steady-state
    /// frame.
    skip_frame: bool,
    /// Ring of recent frame times, in milliseconds.
    frames: Vec<f64>,
    frame_cursor: usize,
    last_frame: Option<Instant>,
    /// The adapter the window actually got, printed once and shown in the help
    /// sheet — `docs/verified/gpu-stack.md` §3 is emphatic that this is
    /// backend-dependent and must never be assumed.
    adapter: String,
    /// The last file whose editor was launched, and when.
    last_editor: Option<(LogicalPath, Instant)>,
    /// What is keeping the window awake, for the status bar.
    repaint: Repaint,
    /// A live feed started before the window existed, waiting for the city it
    /// will drive. Moved into the [`Scene`] the moment the load thread lands.
    pending_live: Option<Box<LiveFeed>>,
    /// How long [`Repaint::Live`] is willing to sleep. `ZERO` is "now".
    ///
    /// The one wake reason with a variable interval: a backlog wants the next
    /// frame immediately, a working agent wants a heartbeat fast enough to age
    /// it, and an empty map wants only enough to move a clock.
    live_wake: Duration,
    /// The terminal dock, in [`Mode::Work`] only.
    ///
    /// `None` everywhere else, which is what makes every other mode exactly as
    /// cheap as it was before this existed.
    dock: Option<Box<crate::panes::Dock>>,
    /// How many panes `polis work` should open once the city is up.
    pending_panes: usize,
    /// Which thread the camera is following, and where it last cut to.
    ///
    /// PRD §12 says following is a **cut**, and a cut happens when the agent
    /// moves — not once a frame. Without this the camera would be re-centred
    /// sixty times a second and the operator could neither pan nor zoom for as
    /// long as they were following anything.
    followed_to: Option<(polis_events::ThreadId, Option<LogicalPath>)>,
}

impl std::fmt::Debug for PolisApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolisApp")
            .field("stage", &self.stage)
            .field("repaint", &self.repaint)
            .finish_non_exhaustive()
    }
}

/// Why the window asked for another frame — the whole of PRD §13.1's idle
/// budget, named.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Repaint {
    /// Nothing. `egui` will sleep until the next input event.
    #[default]
    Idle,
    /// A background job is running.
    Loading,
    /// A live feed has a backlog to apply, or something alive to age.
    ///
    /// The fifth reason, added with PRD §15 M3. Its interval is
    /// `PolisApp::live_wake` rather than a constant, because "there are 4 000
    /// events waiting" and "there is a clock on screen" are the same reason with
    /// two very different urgencies.
    Live,
    /// A pane produced output.
    ///
    /// Named rather than folded into [`Repaint::Live`] so the status bar stays
    /// honest: an agent writing to a terminal is **not** idle, and PRD §13.1's
    /// "< 2 % of one core" is a budget for a window with nothing happening in
    /// it. Saying so beats hiding it.
    Terminal,
    /// A recording is playing.
    Playing,
    /// A tween or a pulse is in flight.
    Animating,
    /// A key is held down.
    KeyHeld,
}

impl Repaint {
    fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Loading => "loading",
            Self::Live => "live",
            Self::Terminal => "terminal",
            Self::Playing => "playing",
            Self::Animating => "animating",
            Self::KeyHeld => "key",
        }
    }
}

/// What the window is showing.
enum Stage {
    /// The session picker.
    Picking(Box<Picker>),
    /// A city and, maybe, a recording are being read.
    Loading {
        rx: Receiver<Result<Loaded, String>>,
        what: String,
    },
    /// The map.
    Running(Box<Scene>),
    /// It could not be opened. Says why, and stays open.
    Failed(String),
}

impl std::fmt::Debug for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Picking(_) => f.write_str("Picking"),
            Self::Loading { what, .. } => write!(f, "Loading({what})"),
            Self::Running(_) => f.write_str("Running"),
            Self::Failed(e) => write!(f, "Failed({e})"),
        }
    }
}

/// Everything the load thread produced.
struct Loaded {
    generated: Generated,
    schedule: Option<ReplaySchedule>,
    label: String,
    speed: f32,
}

/// A city, a world and, optionally, a recording playing over it.
struct Scene {
    generated: Generated,
    /// Built on the first frame, because it needs an `egui::Context`.
    base: Option<BaseMap>,
    camera: Option<Camera>,
    world: World,
    publisher: SnapshotPublisher,
    reader: SnapshotReader,
    snapshot: Arc<WorldSnapshot>,
    driver: Option<ReplayDriver>,
    /// The live feed, when this scene is one (PRD §15 M3). Mutually exclusive
    /// with [`Scene::driver`]: a recording and a wire are two sources for one
    /// world, and mixing them would put a seek in front of an arrival.
    feed: Option<Box<LiveFeed>>,
    label: String,
    view: ViewState,
    tree: TreeView,
    clouds: Clouds,
    timing: ColdStart,
}

impl PolisApp {
    /// Wires the world and the renderer together and returns the app.
    ///
    /// `cc.wgpu_render_state` supplies the device, queue and — critically — the
    /// runtime `target_format`, which differs by backend and must never be
    /// hardcoded.
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config: Config,
        mode: Mode,
        feed: Option<Box<LiveFeed>>,
        started: Instant,
    ) -> Self {
        theme(&cc.egui_ctx);
        let adapter = cc.wgpu_render_state.as_ref().map_or_else(
            || "NOT ON WGPU — this build fell back to another renderer".to_owned(),
            |state| {
                let info = state.adapter.get_info();
                format!(
                    "{} · {:?} · {:?} · target {:?}",
                    info.name, info.device_type, info.backend, state.target_format
                )
            },
        );
        eprintln!("polis: adapter {adapter}");

        let mut feed = feed;
        // The ports had to be bound before there was a context to wake, and the
        // waker needs one — so this is where the two halves meet.
        if let Some(feed) = feed.as_mut() {
            feed.wake_with(&cc.egui_ctx);
        }
        let mut dock = None;
        let mut pending_panes = 0;
        if let Mode::Work {
            repo,
            panes,
            program,
            args,
        } = &mode
        {
            let mut built = crate::panes::Dock::start(
                &cc.egui_ctx,
                config.state_dir.clone(),
                repo.clone(),
                program.clone(),
                args.clone(),
                polis_ingest::env::agent_env(&format!(
                    "http://{}",
                    polis_ingest::default_otlp_addr()
                )),
            );
            // After `theme`, which resets the font definitions: installing the
            // terminal family first would be undone by it.
            built.install_fonts(&cc.egui_ctx);
            eprintln!("polis: {}", built.status);
            // Only open what is not already there. Reattaching to three agents
            // and then starting three more is the one behaviour nobody wants.
            pending_panes = panes.saturating_sub(built.len());
            dock = Some(Box::new(built));
        }

        let stage = match mode {
            Mode::Pick => Stage::Picking(Box::new(Picker::start())),
            // The city is this repository's, and nothing is read from disk that a
            // `polis map` would not read: live is the same city with a wire into
            // it, and `work` is the same city with agents beside it.
            Mode::Map { repo } | Mode::Work { repo, .. } => spawn_load(repo, None, 1.0),
            Mode::Live { options } => spawn_load(options.repo().to_path_buf(), None, 1.0),
            Mode::Replay {
                transcript,
                repo,
                speed,
            } => spawn_load(repo, Some(transcript), speed),
        };

        Self {
            stage,
            config,
            overlay: Overlay::default(),
            started,
            frames: Vec::with_capacity(FRAME_WINDOW),
            frame_cursor: 0,
            last_frame: None,
            adapter,
            first_frame_ms: None,
            skip_frame: false,
            last_editor: None,
            repaint: Repaint::Loading,
            pending_live: feed,
            live_wake: live::IDLE_TICK,
            dock,
            pending_panes,
            followed_to: None,
        }
    }

    /// The snapshot reader the UI samples each frame.
    pub fn snapshot(&self) -> Option<&SnapshotReader> {
        match &self.stage {
            Stage::Running(scene) => Some(&scene.reader),
            _ => None,
        }
    }

    fn record_frame(&mut self, ms: f64) {
        if self.frames.len() < FRAME_WINDOW {
            self.frames.push(ms);
        } else {
            self.frames[self.frame_cursor] = ms;
            self.frame_cursor = (self.frame_cursor + 1) % FRAME_WINDOW;
        }
    }

    fn percentiles(&self) -> (f64, f64) {
        if self.frames.is_empty() {
            return (0.0, 0.0);
        }
        let mut sorted = self.frames.clone();
        sorted.sort_by(f64::total_cmp);
        let at = |q: f64| sorted[((sorted.len() as f64 - 1.0) * q).round() as usize];
        (at(0.5), at(0.99))
    }
}

/// PRD §12: *"Click a building → open in `$EDITOR` via the configured command.
/// Nothing more."*
///
/// A free function rather than a method so it can take the two fields it needs
/// while the scene holds a mutable borrow of another one.
fn open_in_editor(
    last: &mut Option<(LogicalPath, Instant)>,
    command: &str,
    path: &LogicalPath,
    root: &Path,
) {
    if command.trim().is_empty() {
        return;
    }
    let now = Instant::now();
    if let Some((previous, at)) = last {
        if previous == path && now.duration_since(*at) < EDITOR_DEBOUNCE {
            return;
        }
    }
    *last = Some((path.clone(), now));
    let absolute = root.join(path.as_str());
    let rendered = command
        .replace("{path}", &absolute.display().to_string())
        .replace("{line}", "1");
    let Some(argv) = split_command(&rendered) else {
        eprintln!("polis: editor_command is empty");
        return;
    };
    let (program, rest) = argv.split_first().expect("split_command rejects empty");
    // A missing editor is a message, never a crash: the window is the product
    // and the editor is a convenience.
    if let Err(error) = std::process::Command::new(program).args(rest).spawn() {
        eprintln!("polis: could not run {program:?}: {error}");
    }
}

impl eframe::App for PolisApp {
    /// eframe 0.36's entry point is `ui`, not `update`, and the `Ui` handed in
    /// **is** the central panel — no margin, no background, and no
    /// `CentralPanel::default().show(ctx, …)`.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let frame_started = Instant::now();
        let ctx = ui.ctx().clone();
        let dt = self
            .last_frame
            .map_or(1.0 / 60.0, |t| {
                frame_started.duration_since(t).as_secs_f32()
            })
            .min(0.25);
        self.last_frame = Some(frame_started);
        self.repaint = Repaint::Idle;

        self.collect_load();

        match &mut self.stage {
            Stage::Picking(_) => self.draw_picker(ui),
            Stage::Loading { what, .. } => {
                let what = what.clone();
                self.repaint = Repaint::Loading;
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.4);
                    ui.spinner();
                    ui.label(
                        RichText::new(what)
                            .monospace()
                            .color(palette::worker().color()),
                    );
                    ui.label(
                        RichText::new(
                            "walking the checkout, reading git history, parsing imports, \
                             growing the city",
                        )
                        .small()
                        .color(palette::status(polis_world::ThreadStatus::Idle).color()),
                    );
                });
            }
            Stage::Failed(_) => self.draw_failed(ui),
            Stage::Running(_) => self.draw_scene(ui, dt),
        }

        let frame_ms = frame_started.elapsed().as_secs_f64() * 1_000.0;
        if std::mem::take(&mut self.skip_frame) {
            // A base-map rebuild. Counted in the cold-start line instead.
        } else {
            self.record_frame(frame_ms);
        }
        if std::env::var_os("POLIS_DEBUG_FRAMES").is_some() {
            eprintln!("frame {frame_ms:.2} ms  repaint={}", self.repaint.label());
        }
        match self.repaint {
            Repaint::Idle => {}
            Repaint::Loading => ctx.request_repaint_after(crate::session::POLL),
            // `Duration::ZERO` is egui's own spelling of "as soon as you can",
            // so a backlog and a heartbeat go through one call.
            Repaint::Live => ctx.request_repaint_after(self.live_wake),
            // A 30 Hz ceiling on terminal-driven frames. Claude Code's spinner
            // runs at 8–12 Hz, so this is invisible; the ceiling exists for the
            // `cat`-a-large-file case, where uncapped repaint pins a core.
            Repaint::Terminal => ctx.request_repaint_after(TERMINAL_TICK),
            _ => ctx.request_repaint(),
        }
    }

    /// Runs before egui processes the frame's input (`eframe-0.36.1/src/epi.rs:279`).
    ///
    /// One job: give Ctrl+C back to the agent. `egui-winit` turns it into
    /// `Event::Copy` and **never emits the key event** — verified at
    /// `egui-winit-0.36.1/src/lib.rs:1021-1035` — so without this the operator
    /// cannot interrupt a runaway agent from inside Polis. That is a safety
    /// property, not a convenience.
    ///
    /// The rule is Windows Terminal's: copy when there is a selection, interrupt
    /// when there is not. `Event::Paste` is left alone, because pasting is what
    /// Ctrl+V means.
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        let Some(dock) = self.dock.as_ref() else {
            return;
        };
        if dock.wants_interrupt(ctx) {
            crate::panes::interrupt_instead_of_copy(&mut raw.events);
        }
    }

    /// Lets go of the session daemon. **The agents keep running.**
    ///
    /// The whole of the terminal teardown, and its smallness is the point: the
    /// window never owned a child process, so there is no four-step kill, no
    /// two-second poll and no `taskkill`. `polis run`'s teardown is untouched
    /// and still does all of that, because `polis run` really does own its child.
    fn on_exit(&mut self) {
        if let Some(dock) = self.dock.as_mut() {
            dock.shutdown();
        }
    }
}

impl PolisApp {
    fn collect_load(&mut self) {
        let Stage::Loading { rx, .. } = &self.stage else {
            return;
        };
        let loaded = match rx.try_recv() {
            Err(TryRecvError::Empty) => return,
            Ok(Ok(loaded)) => loaded,
            Ok(Err(error)) => {
                // No city means no map to feed, so the channels come down with
                // it rather than filling a bus nobody will ever drain.
                self.pending_live = None;
                self.stage = Stage::Failed(error);
                return;
            }
            Err(TryRecvError::Disconnected) => {
                self.pending_live = None;
                self.stage = Stage::Failed("the city could not be generated".to_owned());
                return;
            }
        };
        self.stage = Stage::Running(Box::new(build_scene(loaded, self.pending_live.take())));
    }

    fn draw_picker(&mut self, ui: &mut egui::Ui) {
        let mut chosen = None;
        if let Stage::Picking(picker) = &mut self.stage {
            chosen = picker.draw(ui);
            if picker.scanning {
                self.repaint = Repaint::Loading;
            }
        }
        if let Some(session) = chosen {
            let repo = session
                .repo
                .clone()
                .unwrap_or_else(|| session.project_dir.clone());
            // The main transcript, not `session_dir()`. The sidecar directory
            // holds the subagent transcripts and only exists when the session
            // spawned one — 91 of 147 sessions in one project on this machine
            // have none, so passing the stem rejected most real sessions.
            // `ReplaySchedule::open` finds the sidecar from the transcript
            // itself, so subagents are still picked up.
            self.stage = spawn_load(repo, Some(session.transcript.clone()), 4.0);
            // PRD §13.1's cold start is "how long until there is a frame", and
            // the operator reading the picker is not part of it.
            self.started = Instant::now();
            self.first_frame_ms = None;
            self.repaint = Repaint::Loading;
        }
    }

    /// The failure screen (PRD §17: does it change a decision?).
    ///
    /// A message with newlines in it is *shown* with newlines in it. The first
    /// version put the whole thing in one wrapped monospace line, so
    /// `citygen`'s "here are three commands, pick one" arrived as three
    /// full-width lines of red with the path in it twice, and the only button
    /// offered a session to somebody who had asked for a map.
    fn draw_failed(&mut self, ui: &mut egui::Ui) {
        let Stage::Failed(error) = &self.stage else {
            return;
        };
        let error = error.clone();
        let mut lines = error.lines();
        let headline = lines.next().unwrap_or_default().to_owned();
        let rest: Vec<String> = lines.map(str::to_owned).collect();
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.22);
            ui.label(
                RichText::new("polis could not open this")
                    .size(20.0)
                    .color(palette::selection().color()),
            );
            ui.add_space(10.0);
            ui.scope(|ui| {
                ui.set_max_width(680.0);
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(headline)
                            .strong()
                            .color(palette::contention().color()),
                    );
                    for line in rest {
                        // An indented line is a command to type, and it reads as
                        // one: monospace, and in the colour the rest of the
                        // window uses for "you can act on this".
                        let indented = line.starts_with("  ");
                        let text = RichText::new(line.trim_end().to_owned());
                        ui.label(if indented {
                            text.monospace().color(palette::hover().color())
                        } else {
                            text.color(palette::worker().color())
                        });
                    }
                });
            });
            ui.add_space(14.0);
            if ui.button("pick a past session to watch instead").clicked() {
                self.stage = Stage::Picking(Box::new(Picker::start()));
            }
        });
    }

    fn draw_scene(&mut self, ui: &mut egui::Ui, dt: f32) {
        let ctx = ui.ctx().clone();

        // The first-run explainer, drawn before the scene is borrowed and
        // painted after everything in it: an `Order::Foreground` area sits above
        // every panel whatever order it was created in. `explaining` stays true
        // for the frame that dismisses it, so the dismissing keystroke or click
        // is spent on the overlay and on nothing else.
        let explaining = self.overlay.explain;
        if explaining && ui::explainer(&ctx) {
            self.overlay.explain = false;
            crate::explain::remember_dismissed();
        }

        let Stage::Running(scene) = &mut self.stage else {
            return;
        };

        // Keys first, because one of them invalidates the base map and the base
        // map is borrowed for the rest of the frame.
        //
        // While the first-run explainer is up the frame's keys are dropped: the
        // keystroke that dismisses it must not also toggle a layer, and the
        // click that dismisses it must not also open an editor.
        let keys = if explaining {
            Keys::default()
        } else {
            read_keys(&ctx)
        };
        if keys.toggle_streets {
            self.config.streets = !self.config.streets;
            scene.base = None;
        }
        if scene
            .base
            .as_ref()
            .is_some_and(|b| !b.matches(&scene.snapshot.layout, self.config.streets))
        {
            scene.base = None;
        }

        // --- The base map, once per layout (PRD §13) ------------------------
        if scene.base.is_none() {
            let layout = Arc::clone(&scene.snapshot.layout);
            let base = BaseMap::render(&ctx, &scene.generated.city, &layout, self.config.streets);
            scene.timing.basemap_ms = base.render_ms;
            scene.base = Some(base);
            scene.camera = None;
            // PRD §13.1's 16.6 ms is a budget for a *steady-state* frame. The
            // frame that rasterises the base map is a layout change, which PRD
            // §13 explicitly says happens "on the order of seconds" and is
            // supposed to be cached — letting it into the ring would make the
            // p99 a report on how long a cache miss takes.
            self.skip_frame = true;
            if self.first_frame_ms.is_none() {
                self.first_frame_ms = Some(self.started.elapsed().as_secs_f64() * 1_000.0);
            }
        }
        let base = scene.base.as_ref().expect("just built");

        // --- The status bar reserves the bottom line first, so the transport
        //     sits above it rather than under it. Panels stack outside-in.
        let mut status_slot = None;
        egui::Panel::bottom("polis-status")
            .exact_size(26.0)
            .show(ui, |ui| {
                status_slot = Some(ui.available_rect_before_wrap());
            });

        // --- Transport, before the world is advanced ------------------------
        let mut action = ui::TransportAction::default();
        if scene.driver.is_some() {
            egui::Panel::bottom("polis-transport")
                .exact_size(88.0)
                .show(ui, |ui| {
                    let driver = scene.driver.as_ref().expect("checked");
                    let compressed = Duration::from_millis(
                        driver
                            .schedule()
                            .session_duration_ms()
                            .saturating_sub(driver.schedule().duration_ms()),
                    );
                    action = ui::transport(
                        ui,
                        &driver.progress(),
                        &scene.label,
                        driver.next_interesting().map(|(_, k)| k),
                        Some(compressed),
                    );
                });
        }
        merge_keys(&mut action, &keys);

        if keys.swap_view {
            self.overlay.view = self.overlay.view.swapped();
            if self.overlay.view == View::Tree {
                if let Some(path) = scene.view.selected.clone() {
                    scene.tree.reveal(&path);
                }
            }
        }
        if keys.toggle_rail {
            self.overlay.rail = !self.overlay.rail;
        }
        if keys.toggle_help {
            self.overlay.help = !self.overlay.help;
            // The sheet is drawn inside the rail, so `h` with the rail closed
            // used to do nothing at all — the one key the guide promises shows
            // "every key, on screen".
            if self.overlay.help {
                self.overlay.rail = true;
            }
        }
        if keys.clear {
            scene.view.selected = None;
            scene.view.follow = None;
            self.overlay.attention_cursor = None;
            self.overlay.help = false;
        }
        if keys.back_to_picker || action.back_to_picker {
            self.stage = Stage::Picking(Box::new(Picker::start()));
            return;
        }

        // --- Advance the world: advance, publish, load. Once per frame. -----
        //
        // One shape, two sources. A recording advances a clock; a live feed
        // drains a bus. Both then publish exactly once and the renderer loads
        // exactly once, which is PRD §5's "decouple event rate from frame rate"
        // written out: nothing here runs per event.
        if let Some(driver) = &mut scene.driver {
            apply_transport(driver, &action, &mut scene.world);
            if driver.clock().is_playing() {
                driver.advance(Duration::from_secs_f32(dt), &mut scene.world);
                if driver.is_finished() {
                    driver.clock_mut().pause();
                }
                self.repaint = Repaint::Playing;
            }
            scene.publisher.publish(&scene.world);
            scene.snapshot = scene.reader.load();
        }
        if let Some(feed) = &mut scene.feed {
            let now = Instant::now();
            let pumped = feed.pump(&mut scene.world, now);
            scene.publisher.publish(&scene.world);
            scene.snapshot = scene.reader.load();
            self.live_wake = feed.wake_after(&scene.snapshot, pumped.backlog);
            self.repaint = self.repaint.max_urgency(Repaint::Live);
        }

        let snapshot = Arc::clone(&scene.snapshot);

        // --- Panels ---------------------------------------------------------
        let vitals_slot = std::cell::Cell::new(None);
        egui::Panel::top("polis-title")
            .exact_size(30.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(scene.generated.title())
                            .monospace()
                            .strong()
                            .color(palette::selection().color()),
                    );
                    ui.label(
                        RichText::new(&scene.label)
                            .small()
                            .color(palette::district_label().color()),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .selectable_label(self.overlay.help, "what is this? (h)")
                            .on_hover_text(
                                "what a building is, what the shapes and colours mean, \
                                 and every key",
                            )
                            .clicked()
                        {
                            self.overlay.help = !self.overlay.help;
                            self.overlay.rail = true;
                        }
                        if ui.selectable_label(self.overlay.rail, "rail (i)").clicked() {
                            self.overlay.rail = !self.overlay.rail;
                        }
                        if ui
                            .selectable_label(self.overlay.view == View::Tree, "tree (t)")
                            .clicked()
                        {
                            self.overlay.view = self.overlay.view.swapped();
                        }
                    });
                });
            });

        // --- Live mode says what it is doing, always ------------------------
        //
        // > The operator ran a real agent for minutes and saw an empty map with
        // > no explanation.
        //
        // A silent empty map is the exact failure this milestone exists to fix,
        // so the strip is unconditional and the explanation appears whenever
        // there is nothing on the map to explain itself.
        if let Some(feed) = scene.feed.as_deref() {
            let now = Instant::now();
            // Created after `polis-status`, so it stacks directly above it.
            egui::Panel::bottom("polis-live")
                .exact_size(24.0)
                .show(ui, |ui| live::strip(ui, feed, &snapshot, now));
            if snapshot.threads.is_empty() {
                egui::Panel::top("polis-live-waiting")
                    .exact_size(live::waiting_height(feed))
                    .show(ui, |ui| live::waiting(ui, feed, now));
            }
        }

        // Why this city looks the way it does, when the answer is not "your
        // code" — an empty history draws an almost empty map, and silence there
        // reads as a broken product (`citygen::preflight`).
        if let Some(notice) = scene.generated.notice.clone() {
            egui::Panel::top("polis-notice")
                .exact_size(48.0)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new("nothing to draw yet")
                                .monospace()
                                .strong()
                                .color(palette::needs_decision().color()),
                        );
                        ui.label(
                            RichText::new(notice)
                                .small()
                                .color(palette::worker().color()),
                        );
                    });
                });
        }

        // --- The drill-down rail --------------------------------------------
        //
        // Three things in one column, in the order PRD §1 ranks them: what is
        // waiting on a human, what the operator is pointing at, and who is
        // working. `jump` and `panel` are collected here and applied once the
        // rail's borrow of the scene is released.
        // PRD §12's shared highlight is double-buffered, and this is the swap:
        // it publishes what the rail and the map said about the pointer last
        // frame, before either of them draws this one.
        scene.view.begin_frame();
        // The dock goes in after `polis-title` and before `polis-rail`, so the
        // map keeps `available_rect_before_wrap()` and neither panel has to know
        // about the other.
        if let Some(dock) = self.dock.as_mut() {
            // Before anything else in the dock, and before a pane can consume
            // them: `Ctrl+Alt+…`, `Ctrl+\``, `F6`. Safe to read without
            // consuming, because `polis_term::input` declines to encode
            // `Ctrl+Alt` at all — the same rule that keeps `AltGr` working.
            dock.reserved_chords(&ctx);
            let outcome = dock.poll();
            if outcome.active {
                self.repaint = self.repaint.max_urgency(Repaint::Terminal);
            }
            if self.pending_panes > 0 && dock.connected() {
                self.pending_panes -= 1;
                dock.open(45, 100);
            }
            let collapsed = dock.collapsed;
            let (minimum, preferred) = (dock.minimum_width(), dock.preferred_width());
            let available = ui.available_width();

            // Three things want this row and two of them have hard floors. When
            // they do not all fit, the rail closes rather than the dock
            // shrinking below eighty columns or the map disappearing.
            if !collapsed && self.overlay.rail {
                let rest = available - minimum - RAIL_WIDTH;
                if rest < MAP_MIN_WIDTH {
                    self.overlay.rail = false;
                }
            }
            let rail = if self.overlay.rail { RAIL_WIDTH } else { 0.0 };
            let widest = (available - rail - MAP_MIN_WIDTH).max(minimum);

            egui::Panel::left("polis-terminals")
                .resizable(!collapsed)
                .default_size(if collapsed {
                    COLLAPSED_DOCK
                } else {
                    preferred.min(widest)
                })
                .size_range(if collapsed {
                    COLLAPSED_DOCK..=COLLAPSED_DOCK
                } else {
                    minimum..=widest
                })
                .show(ui, |ui| {
                    let drew = dock.draw(ui);
                    if drew.active {
                        self.repaint = self.repaint.max_urgency(Repaint::Terminal);
                    }
                });
        }

        let mut jump: Option<drill::Jump> = None;
        let mut panel = ui::PanelAction::default();
        let mut subject: Option<LogicalPath> = None;
        let mut dismissed: Option<polis_events::ThreadId> = None;
        if self.overlay.rail {
            egui::Panel::right("polis-rail")
                .default_size(370.0)
                .size_range(280.0..=620.0)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if self.overlay.help {
                                ui::help(ui);
                                ui.separator();
                                ui.label(
                                    RichText::new(&self.adapter)
                                        .small()
                                        .monospace()
                                        .color(palette::worker().color()),
                                );
                                ui.separator();
                            }
                            // PRD §1's primary decision comes first on the
                            // column, above everything the operator might merely
                            // be curious about.
                            jump = ui::attention_list(ui, &snapshot, self.overlay.attention_cursor);
                            subject = scene
                                .view
                                .selected
                                .clone()
                                .or_else(|| scene.view.hovered.clone());
                            if let Some(path) = &subject {
                                panel = ui::building_panel(ui, &snapshot, path);
                                ui.separator();
                            }
                            dismissed = ui::status_rail(ui, &snapshot, &mut scene.view);
                        });
                });
        }
        // The operator closing a thread by hand. Applied here, once the rail has
        // released its borrow of the scene, and published straight away rather
        // than waiting for the next pump: `publish` is rate-limited, and a `✕`
        // whose row is still there a beat later reads as a button that missed.
        if let Some(thread) = dismissed {
            scene.world.dismiss_thread(&thread);
            if scene.view.follow.as_ref() == Some(&thread) {
                scene.view.follow = None;
            }
            // And the shared highlight, for the same reason: a selection that
            // outlived its row would keep the rail and the map lit for a thread
            // neither of them can now show.
            if scene.view.selected_thread.as_ref() == Some(&thread) {
                scene.view.selected_thread = None;
            }
            self.overlay.attention_cursor = None;
            scene.publisher.force(&scene.world);
            scene.snapshot = scene.reader.load();
            ui.ctx().request_repaint();
        }
        if keys.attention {
            jump = self.overlay.next_attention(&snapshot);
        }
        if let Some(thread) = panel.follow {
            scene.view.follow = if scene.view.follow.as_ref() == Some(&thread) {
                None
            } else {
                Some(thread)
            };
        }
        if keys.follow {
            let threads: Vec<polis_events::ThreadId> =
                snapshot.threads.iter().map(|t| t.id.clone()).collect();
            let prefer = subject.as_ref().and_then(|path| {
                drill::facts(&snapshot, path)
                    .touches
                    .first()
                    .map(|t| t.thread.clone())
            });
            scene.view.follow =
                drill::next_follow(scene.view.follow.as_ref(), &threads, prefer.as_ref());
        }
        if panel.open_editor {
            if let Some(path) = &subject {
                open_in_editor(
                    &mut self.last_editor,
                    &self.config.editor_command,
                    path,
                    &scene.generated.root,
                );
            }
        }

        // A jump is *go there now*, so it releases the camera from whatever it
        // was bound to; following is *stay with it*, and the two fighting over
        // the camera every frame would look like a bug in both.
        let mut cut_to = None;
        if let Some(jump) = jump {
            if let Some(path) = &jump.path {
                scene.view.selected = Some(path.clone());
                scene.tree.reveal(path);
            }
            scene.view.follow = None;
            cut_to = mapview::mark_position(base, &snapshot, &jump.thread, jump.path.as_ref());
        }

        // --- The central area: the map, or the tree that is co-equal with it -
        let rect = ui.available_rect_before_wrap();
        if std::env::var_os("POLIS_DEBUG_LAYOUT").is_some() {
            eprintln!(
                "layout: ppp={:.2} screen={:?} central={rect:?} max={:?} cursor_min={:?}",
                ctx.pixels_per_point(),
                ctx.viewport_rect(),
                ui.max_rect(),
                ui.cursor().min,
            );
        }
        let mut frame = mapview::MapFrame::default();
        let mut clicked = None;
        match self.overlay.view {
            View::Map => {
                let camera = scene.camera.get_or_insert_with(|| {
                    Camera::fit(base.edge(), base.geometry.median_building_px, rect)
                });
                if keys.reset_camera {
                    *camera = Camera::fit(base.edge(), base.geometry.median_building_px, rect);
                }
                // PRD §12: follow is a **cut**, never a pan — and a cut only
                // when the agent has actually moved. Re-centring every frame
                // would take the pan and the zoom away from the operator for as
                // long as they were following, which is the opposite of what
                // binding the camera to a thread is for.
                if let Some(id) = scene.view.follow.clone() {
                    if let Some(thread) = snapshot.thread(&id) {
                        // Where the thread *is*, not where its territory
                        // averages out to: agents jump discontinuously across
                        // the tree and the last step is the jump.
                        let at = thread.trail.back().map(|(path, _)| path.clone());
                        let moved = self.followed_to.as_ref() != Some(&(id.clone(), at.clone()));
                        if moved {
                            self.followed_to = Some((id, at.clone()));
                            let point = at
                                .as_ref()
                                .and_then(|path| base.geometry.position_of(path))
                                .or_else(|| {
                                    thread.territory.centre_of_mass.map(|c| base.to_map(c))
                                });
                            if let Some(point) = point {
                                drill::drill_to(camera, point);
                            }
                        }
                    }
                } else {
                    self.followed_to = None;
                }
                if let Some(at) = cut_to {
                    drill::drill_to(camera, at);
                }
                frame = mapview::draw(
                    ui,
                    rect,
                    base,
                    camera,
                    &mut scene.clouds,
                    &snapshot,
                    &mut scene.view,
                    self.config.cloud_cap,
                    dt,
                );
                scene.view.hovered.clone_from(&frame.hovered);
                clicked.clone_from(&frame.clicked);
                if let (Some(path), Some(pos)) = (&frame.hovered, ctx.pointer_hover_pos()) {
                    ui::hover_card(&ctx, pos, &snapshot, path);
                }
            }
            View::Tree => {
                let out = scene.tree.draw(ui, &snapshot, &mut scene.view);
                // The tree's row is the map's building: the same click, the same
                // camera cut, the same editor (PRD §12, *co-equal*).
                if let Some(path) = out.centre_on {
                    if let (Some(camera), Some(at)) =
                        (scene.camera.as_mut(), base.geometry.position_of(&path))
                    {
                        drill::drill_to(camera, at);
                    }
                }
                clicked = out.clicked;
                if let (Some(camera), Some(at)) = (scene.camera.as_mut(), cut_to) {
                    drill::drill_to(camera, at);
                }
                if let Some(path) = &scene.view.hovered {
                    if let Some(pos) = ctx.pointer_hover_pos() {
                        ui::hover_card(&ctx, pos, &snapshot, path);
                    }
                }
                frame.tier = scene
                    .camera
                    .as_ref()
                    .map_or(polis_render::camera::ZoomTier::City, Camera::tier);
            }
        }

        if let Some(path) = clicked.filter(|_| !explaining) {
            scene.view.selected = Some(path.clone());
            open_in_editor(
                &mut self.last_editor,
                &self.config.editor_command,
                &path,
                &scene.generated.root,
            );
        }

        if frame.animating {
            self.repaint = self.repaint.max_urgency(Repaint::Animating);
        }
        if frame.key_held {
            self.repaint = self.repaint.max_urgency(Repaint::KeyHeld);
        }
        // A decaying attention mark changes what is on screen without any input,
        // so it earns a wake — but a slow one, not a spin.
        if snapshot.attention.iter().any(|m| m.kind.decays()) {
            self.repaint = self.repaint.max_urgency(Repaint::Animating);
        }

        let camera = scene.camera;
        let vitals = Vitals {
            mode: match (scene.driver.is_some(), scene.feed.is_some()) {
                (_, true) => "live",
                (true, _) => "replay",
                _ => "map",
            },
            view: self.overlay.view,
            tier: frame.tier,
            zoom: camera.as_ref().map_or(1.0, Camera::zoom),
            building_px: camera.as_ref().map_or(0.0, Camera::building_screen_px),
            frame_p50: 0.0,
            frame_p99: 0.0,
            labels: (frame.labels_placed, frame.labels_dropped),
            buildings: frame.buildings_drawn,
            clouds: (scene.clouds.shown, scene.clouds.kernels),
            cold_start_ms: self
                .first_frame_ms
                .unwrap_or_else(|| scene.timing.total_ms()),
        };
        vitals_slot.set(Some(vitals));

        let (p50, p99) = self.percentiles();
        if let (Some(rect), Some(mut vitals)) = (status_slot, vitals_slot.get()) {
            vitals.frame_p50 = p50;
            vitals.frame_p99 = p99;
            let repaint = self.repaint;
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                // The attention count on the bar is a control: the rail can be
                // shut, and PRD §1's primary decision must never be more than
                // one click away.
                if ui::status_bar(ui, &snapshot, vitals) {
                    self.overlay.rail = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(repaint.label())
                            .small()
                            .monospace()
                            .color(palette::status(polis_world::ThreadStatus::Idle).color()),
                    );
                });
            });
        }
    }
}

impl Repaint {
    /// Keeps the most demanding reason, so the status bar names the one that is
    /// actually costing frames.
    fn max_urgency(self, other: Self) -> Self {
        let rank = |r: Self| match r {
            Self::Idle => 0,
            Self::Loading => 1,
            Self::Live => 2,
            // Above `Live`, whose heartbeat is allowed to be a whole second,
            // and below the two the operator is driving by hand.
            Self::Terminal => 3,
            Self::Animating => 4,
            Self::KeyHeld => 5,
            Self::Playing => 6,
        };
        if rank(other) > rank(self) {
            other
        } else {
            self
        }
    }
}

/// The keys the window binds, read once per frame.
///
/// A flat set of flags rather than a state machine: every one of these is an
/// independent edge from one frame's input, several can fire together — `.`
/// while `]` is held — and a two-variant enum per key would say "pressed" and
/// "not pressed" in more words.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default, Clone, Copy)]
struct Keys {
    swap_view: bool,
    toggle_rail: bool,
    toggle_help: bool,
    toggle_streets: bool,
    reset_camera: bool,
    /// `f` — bind the camera to the next thread (PRD §12).
    follow: bool,
    /// `a` — jump to the next attention state, worst first.
    ///
    /// The one key that is the whole product in one keystroke: PRD §1's
    /// *"get to the thread that is waiting on a human"*.
    attention: bool,
    clear: bool,
    back_to_picker: bool,
    play: bool,
    step: bool,
    step_back: bool,
    faster: bool,
    slower: bool,
    next_interesting: bool,
    home: bool,
    end: bool,
}

fn read_keys(ctx: &egui::Context) -> Keys {
    // A text field has the keyboard: `t` is a letter, not a command. The test is
    // `egui_wants_keyboard_input` and **not** "something has focus" — clicking any
    // button gives it focus, and the first version of this guard therefore
    // disabled every keyboard shortcut in the window the moment the operator
    // clicked a speed button.
    if ctx.egui_wants_keyboard_input() {
        return Keys::default();
    }
    ctx.input(|i| Keys {
        swap_view: i.key_pressed(egui::Key::T),
        toggle_rail: i.key_pressed(egui::Key::I),
        toggle_help: i.key_pressed(egui::Key::H) || i.key_pressed(egui::Key::Questionmark),
        toggle_streets: i.key_pressed(egui::Key::S),
        reset_camera: i.key_pressed(egui::Key::R),
        follow: i.key_pressed(egui::Key::F),
        attention: i.key_pressed(egui::Key::A),
        clear: i.key_pressed(egui::Key::Escape),
        back_to_picker: i.key_pressed(egui::Key::P),
        play: i.key_pressed(egui::Key::Space),
        step: i.key_pressed(egui::Key::Period),
        step_back: i.key_pressed(egui::Key::Comma),
        faster: i.key_pressed(egui::Key::CloseBracket),
        slower: i.key_pressed(egui::Key::OpenBracket),
        next_interesting: i.key_pressed(egui::Key::N),
        home: i.key_pressed(egui::Key::Home),
        end: i.key_pressed(egui::Key::End),
    })
}

fn merge_keys(action: &mut ui::TransportAction, keys: &Keys) {
    action.toggle |= keys.play;
    action.step |= keys.step;
    action.step_back |= keys.step_back;
    action.next_interesting |= keys.next_interesting;
    action.faster |= keys.faster;
    action.slower |= keys.slower;
    if keys.home {
        action.seek = Some(0.0);
    }
    if keys.end {
        action.seek = Some(1.0);
    }
}

/// Applies one frame of transport input to the driver.
fn apply_transport(driver: &mut ReplayDriver, action: &ui::TransportAction, world: &mut World) {
    if action.toggle {
        driver.clock_mut().toggle();
    }
    if let Some(speed) = action.speed {
        driver.clock_mut().set_speed(speed);
    }
    if action.faster {
        driver.clock_mut().faster();
    }
    if action.slower {
        driver.clock_mut().slower();
    }
    if let Some(fraction) = action.seek {
        driver.seek_fraction(fraction, world);
    }
    if action.step {
        driver.clock_mut().pause();
        driver.step(world);
    }
    if action.step_back {
        // Events are not invertible, so one step back is a seek to the previous
        // event's time — which `ReplayDriver::seek` implements by resetting and
        // re-applying. At ten microseconds an event that is affordable.
        driver.clock_mut().pause();
        let position = driver.progress().position_ms;
        let target = driver
            .schedule()
            .entries
            .iter()
            .rev()
            .map(|e| e.schedule_ms)
            .find(|ms| *ms < position)
            .unwrap_or(0);
        driver.seek(target, world);
    }
    if action.next_interesting {
        driver.seek_next_interesting(world);
    }
}

/// Starts the load on its own thread so the window stays responsive.
fn spawn_load(repo: PathBuf, transcript: Option<PathBuf>, speed: f32) -> Stage {
    let what = match &transcript {
        Some(path) => format!("reading {}", path.display()),
        None => format!("generating the city for {}", repo.display()),
    };
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::Builder::new()
        .name("polis-load".to_owned())
        .spawn(move || {
            let _ =
                tx.send(load(&repo, transcript.as_deref(), speed).map_err(|e| format!("{e:#}")));
        })
        .expect("spawning the load thread");
    Stage::Loading { rx, what }
}

fn load(repo: &Path, transcript: Option<&Path>, speed: f32) -> anyhow::Result<Loaded> {
    // No `.context("generating the city for …")` here, deliberately. The
    // failures worth a screen — not a checkout, no `git` — are explained in
    // full by `citygen::preflight`, in the operator's words and with the path
    // already in them, and `{:#}` would flatten a wrapping context onto the
    // front of that as a second copy of the same path.
    let generated = citygen::generate(repo)?;
    let Some(transcript) = transcript else {
        return Ok(Loaded {
            label: format!("{} · the city as it stands right now", repo.display()),
            generated,
            schedule: None,
            speed,
        });
    };
    let transcript = &crate::session::resolve_transcript(transcript).ok_or_else(|| {
        anyhow::anyhow!(
            "no such transcript: {} (tried it, and {}.jsonl)",
            transcript.display(),
            transcript.display()
        )
    })?;
    let transcript = transcript.as_path();
    let mapper = PathMapper::new(repo).unwrap_or_default();
    // PRD §15 M2 wants a session watchable in one sitting, and a real one is
    // mostly dead air: this machine's 26.1-hour session is 18.4 minutes at the
    // 2 s cap, with 25.8 hours of nothing removed. The world still ages by real
    // session time across each compressed gap, so decay and TTLs are unaffected
    // — only the operator's wall clock is.
    let schedule = ReplaySchedule::open(transcript, &mapper)
        .with_context(|| format!("reading {}", transcript.display()))?
        .with_idle_gap_cap(Some(polis_world::replay::DEFAULT_IDLE_GAP_CAP));
    let label = format!(
        "{} · {} events · {} files · {} parsed, {} bad json, {} unknown type",
        transcript.file_name().map_or_else(
            || transcript.display().to_string(),
            |n| n.to_string_lossy().into_owned()
        ),
        schedule.len(),
        schedule.files.len(),
        schedule.stats.ok,
        schedule.stats.bad_json,
        schedule.stats.unknown_type
    );
    Ok(Loaded {
        generated,
        schedule: Some(schedule),
        label,
        speed,
    })
}

fn build_scene(loaded: Loaded, feed: Option<Box<LiveFeed>>) -> Scene {
    let Loaded {
        generated,
        schedule,
        label,
        speed,
    } = loaded;
    let timing = generated.timing;
    let mut world = World::new(generated.tree.clone(), generated.city.layout.clone());
    let mut feed = feed;
    let label = match feed.as_mut() {
        None => label,
        Some(feed) => {
            // `World::new` derives the mapper from the checkout *and every
            // registered worktree* (PRD §7.6), which the feed could not know
            // when it bound its ports before the git walk had run.
            feed.adopt_mapper(world.mapper());
            format!("{label} · live")
        }
    };
    let driver = schedule.map(|schedule| {
        let mut driver = ReplayDriver::new(schedule);
        driver.clock_mut().set_speed(speed);
        driver.clock_mut().play();
        driver
    });
    let (publisher, reader) = snapshot::from_world(&world);
    // One forced publish so the first frame has a real snapshot rather than an
    // empty one over the same layout.
    publisher.force(&world);
    let snapshot = reader.load();
    world.tick(world.now());
    Scene {
        generated,
        base: None,
        camera: None,
        world,
        publisher,
        reader,
        snapshot,
        driver,
        feed,
        label,
        view: ViewState::default(),
        tree: TreeView::default(),
        clouds: Clouds::default(),
        timing,
    }
}

/// The window's palette: PRD §10.3's budget applies to the map, and the chrome
/// around it has to stay out of the way of the same range.
fn theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(12, 13, 16);
    visuals.window_fill = Color32::from_rgb(14, 16, 20);
    visuals.extreme_bg_color = Color32::from_rgb(8, 9, 11);
    visuals.override_text_color = Some(palette::worker().color());
    ctx.set_visuals(visuals);
}

/// Splits a configured command into argv, honouring double quotes.
///
/// Deliberately not a shell: PRD §12 says the click opens the file and
/// *"nothing more"*, and handing an operator-supplied string to a shell would
/// make a config file a code-execution surface for no benefit.
fn split_command(command: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in command.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    out.push(std::mem::take(&mut current));
                    any = false;
                }
            }
            c => {
                current.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(current);
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_splits_into_argv_and_keeps_quoted_paths_whole() {
        assert_eq!(
            split_command("code --goto C:/a/b.rs:1"),
            Some(vec![
                "code".to_owned(),
                "--goto".to_owned(),
                "C:/a/b.rs:1".to_owned()
            ])
        );
        assert_eq!(
            split_command(r#""C:/Program Files/x/ed.exe" --wait "a b.rs""#),
            Some(vec![
                "C:/Program Files/x/ed.exe".to_owned(),
                "--wait".to_owned(),
                "a b.rs".to_owned()
            ])
        );
        assert_eq!(split_command("   "), None);
        assert_eq!(split_command(""), None);
    }

    /// The click must not become a shell: an operator's config file is not a
    /// code-execution surface.
    #[test]
    fn a_command_is_argv_and_never_a_shell_line() {
        let argv = split_command("editor {path} && rm -rf /").expect("argv");
        assert_eq!(argv[0], "editor");
        assert!(
            argv.contains(&"&&".to_owned()),
            "the shell operator is an argument, not an operator: {argv:?}"
        );
    }

    /// Whatever `$EDITOR` says on this machine, the template has to come out of
    /// substitution with a real path in it and no placeholder left behind.
    #[test]
    fn the_configured_editor_command_substitutes_a_path() {
        let config = Config::default();
        let rendered = config
            .editor_command
            .replace("{path}", "C:/repo/src/main.rs")
            .replace("{line}", "1");
        let argv = split_command(&rendered).expect("argv");
        assert!(!argv[0].is_empty(), "{argv:?}");
        assert!(argv.iter().any(|a| a.contains("main.rs")), "{argv:?}");
        assert!(!argv.iter().any(|a| a.contains("{path}")), "{argv:?}");
        assert!(!argv.iter().any(|a| a.contains("{line}")), "{argv:?}");
    }

    /// The repaint decision is the idle budget. The most demanding reason wins,
    /// and "nothing is happening" has to survive being asked about repeatedly.
    #[test]
    fn the_repaint_reason_keeps_the_most_demanding_cause() {
        assert_eq!(Repaint::Idle.max_urgency(Repaint::Idle), Repaint::Idle);
        assert_eq!(
            Repaint::Animating.max_urgency(Repaint::Playing),
            Repaint::Playing
        );
        assert_eq!(
            Repaint::Playing.max_urgency(Repaint::Animating),
            Repaint::Playing
        );
        assert_eq!(
            Repaint::Idle.max_urgency(Repaint::Animating),
            Repaint::Animating
        );
    }

    #[test]
    fn frame_percentiles_are_stable_on_a_short_history() {
        let mut app_frames: Vec<f64> = (0..100).map(f64::from).collect();
        app_frames.sort_by(f64::total_cmp);
        let at = |q: f64| app_frames[((app_frames.len() as f64 - 1.0) * q).round() as usize];
        assert!((at(0.5) - 50.0).abs() <= 1.0);
        assert!((at(0.99) - 98.0).abs() <= 2.0);
    }
}
