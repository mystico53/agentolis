//! `polis-app` — the application shell (PRD §12, §13, §14).
//!
//! Owns the window, the UI overlay, configuration and the CLI. This is where the
//! ingest threads, the world thread and the render callback are wired together.
//!
//! # Threading
//!
//! winit requires the event loop on the **main thread**, so `main` stays
//! synchronous and every other part runs beside it:
//!
//! ```text
//! main thread     eframe/winit event loop, egui overlay, render callback
//! world thread    single writer of World, publishes a snapshot per batch
//! otel runtime    private 2-worker tokio runtime, named thread
//! hook thread     blocking recv_from loop
//! fs thread       notify watcher
//! tail thread     JSONL tailer
//! layout thread   incremental growth, never inside a frame
//! ```
//!
//! There is no `#[tokio::main]` anywhere in Polis.

pub mod cli;
pub mod commands;
pub mod config;
pub mod format;
pub mod ui;

#[cfg(test)]
pub(crate) mod testutil;

use eframe::egui;
use polis_world::snapshot::SnapshotReader;

/// The eframe application.
#[derive(Debug)]
pub struct PolisApp {
    _private: (),
}

impl PolisApp {
    /// Wires ingest, world and renderer together and returns the app.
    ///
    /// `cc.wgpu_render_state` supplies the device, queue and — critically — the
    /// runtime `target_format`, which differs by backend and must never be
    /// hardcoded. The renderer is inserted into
    /// `renderer.write().callback_resources`, because egui-wgpu's `paint()`
    /// takes `RenderPass<'static>` and cannot borrow from the callback.
    pub fn new(cc: &eframe::CreationContext<'_>, config: config::Config) -> anyhow::Result<Self> {
        let _ = (cc, config);
        todo!("PRD §13 — stash PolisRenderer in egui-wgpu's type-map, start ingest")
    }

    /// The snapshot reader the UI samples each frame.
    pub fn snapshot(&self) -> &SnapshotReader {
        todo!("PRD §5 — one load per frame, never per event")
    }
}

impl eframe::App for PolisApp {
    /// eframe 0.36's entry point is `ui`, not `update`, and the `Ui` handed in
    /// **is** the central panel — no margin, no background, and no
    /// `CentralPanel::default().show(...)`.
    ///
    /// Order matters: the custom wgpu layer is added to the painter first and the
    /// egui text after, so text composites on top by submission order.
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let _ = (ui, frame);
        todo!("PRD §13 — paint callback first, then the text overlay on top")
    }
}

/// Launches the window (PRD §13).
///
/// `Renderer::Wgpu`, never glow. Note that `glow` appearing in the dependency
/// tree does **not** mean the glow renderer is in use: it arrives via
/// `wgpu-hal`'s GLES backend.
pub fn run(config: config::Config) -> anyhow::Result<()> {
    // Taken by value because `run` owns the configuration for the process
    // lifetime; dropping it here is what a `todo!()` body can honestly do with it.
    drop(config);
    todo!("PRD §13 — eframe::run_native with Renderer::Wgpu")
}
