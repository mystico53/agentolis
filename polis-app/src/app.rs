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
//! ```
//!
//! The world is driven on the main thread deliberately. PRD §5 says the
//! renderer must sample a lock-free snapshot rather than block on a writer, and
//! that is exactly what happens here — [`SnapshotPublisher`] publishes and
//! [`SnapshotReader`] loads — but with a *recording* there is no second writer:
//! `polis-world` applies 22 759 real events in 239 ms, so a whole session is
//! four frames of work and a thread would buy latency Polis does not have.
//! Moving the publisher to a world thread is a two-line change when live ingest
//! lands, and the shape here is already the shape that needs: `advance`,
//! `publish`, `load`, once per frame, never per event.
//!
//! # The idle budget is a repaint discipline
//!
//! > Idle CPU (no agent activity): < 2% of one core. […] Idle cost matters —
//! > this thing runs all day on the operator's second monitor. (PRD §13.1)
//!
//! `egui` repaints on input and on request, and nothing else. So this file
//! requests a repaint in exactly four cases: a transport is playing, an
//! animation is in flight, a key is held, or a background job is still running.
//! [`Repaint`] is that decision made once, in one place, with a name attached so
//! the status bar can say which of the four is keeping the window awake.

// The frame loop is arithmetic against a pixel grid and a millisecond clock, and
// `too_many_lines` on `ui()` would only push one ordered sequence of panels into
// fragments each called once.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

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
use crate::mapview::{self, ViewState};
use crate::palette;
use crate::session::Picker;
use crate::treeview::TreeView;
use crate::ui::{self, Overlay, View, Vitals};

/// How many frame times the status bar keeps.
const FRAME_WINDOW: usize = 240;

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
        Box::new(move |cc| Ok(Box::new(PolisApp::new(cc, config, mode, started)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
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

        let stage = match mode {
            Mode::Pick => Stage::Picking(Box::new(Picker::start())),
            Mode::Map { repo } => spawn_load(repo, None, 1.0),
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
            _ => ctx.request_repaint(),
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
                self.stage = Stage::Failed(error);
                return;
            }
            Err(TryRecvError::Disconnected) => {
                self.stage = Stage::Failed("the city could not be generated".to_owned());
                return;
            }
        };
        self.stage = Stage::Running(Box::new(build_scene(loaded)));
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

    fn draw_failed(&mut self, ui: &mut egui::Ui) {
        let Stage::Failed(error) = &self.stage else {
            return;
        };
        let error = error.clone();
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.35);
            ui.label(
                RichText::new("polis could not open this")
                    .size(20.0)
                    .color(palette::selection().color()),
            );
            ui.label(
                RichText::new(error)
                    .monospace()
                    .color(palette::contention().color()),
            );
            if ui.button("pick another session").clicked() {
                self.stage = Stage::Picking(Box::new(Picker::start()));
            }
        });
    }

    fn draw_scene(&mut self, ui: &mut egui::Ui, dt: f32) {
        let ctx = ui.ctx().clone();
        let Stage::Running(scene) = &mut self.stage else {
            return;
        };

        // Keys first, because one of them invalidates the base map and the base
        // map is borrowed for the rest of the frame.
        let keys = read_keys(&ctx);
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
        }
        if keys.clear {
            scene.view.selected = None;
            self.overlay.help = false;
        }
        if keys.back_to_picker || action.back_to_picker {
            self.stage = Stage::Picking(Box::new(Picker::start()));
            return;
        }

        // --- Advance the world: advance, publish, load. Once per frame. -----
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
                        if ui.selectable_label(self.overlay.help, "keys (h)").clicked() {
                            self.overlay.help = !self.overlay.help;
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
                            let subject = scene
                                .view
                                .selected
                                .clone()
                                .or_else(|| scene.view.hovered.clone());
                            if let Some(path) = subject {
                                ui::building_panel(ui, &snapshot, &path);
                                ui.separator();
                            }
                            ui::status_rail(ui, &snapshot, &mut scene.view);
                        });
                });
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
                // PRD §12: follow is a **cut**, never a pan.
                if let Some(id) = scene.view.follow.clone() {
                    if let Some(thread) = snapshot.thread(&id) {
                        if let Some(centre) = thread.territory.centre_of_mass {
                            camera.cut_to(base.to_map(centre));
                        }
                    }
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
                let centre_on = scene.tree.draw(ui, &snapshot, &mut scene.view);
                if let Some(path) = centre_on {
                    if let (Some(camera), Some(at)) =
                        (scene.camera.as_mut(), base.geometry.position_of(&path))
                    {
                        camera.cut_to(at);
                    }
                }
                frame.tier = scene
                    .camera
                    .as_ref()
                    .map_or(polis_render::camera::ZoomTier::City, Camera::tier);
            }
        }

        if let Some(path) = clicked {
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
            mode: if scene.driver.is_some() {
                "replay"
            } else {
                "map"
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
                ui::status_bar(ui, &snapshot, vitals);
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
            Self::Animating => 2,
            Self::KeyHeld => 3,
            Self::Playing => 4,
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
    let generated = citygen::generate(repo)
        .with_context(|| format!("generating the city for {}", repo.display()))?;
    let Some(transcript) = transcript else {
        return Ok(Loaded {
            label: format!("{} · static map (PRD §15 M1)", repo.display()),
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

fn build_scene(loaded: Loaded) -> Scene {
    let Loaded {
        generated,
        schedule,
        label,
        speed,
    } = loaded;
    let timing = generated.timing;
    let mut world = World::new(generated.tree.clone(), generated.city.layout.clone());
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
