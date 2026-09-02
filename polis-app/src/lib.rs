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

pub mod app;
pub mod basemap;
pub mod camera;
pub mod citygen;
pub mod cli;
pub mod clouds;
pub mod commands;
pub mod config;
pub mod format;
pub mod labels;
pub mod mapview;
pub mod palette;
pub mod session;
pub mod snapshot;
pub mod treeview;
pub mod ui;

#[cfg(test)]
pub(crate) mod testutil;

pub use app::{launch, Mode, PolisApp};

/// Launches the window over the current repository (PRD §13, §15 M1).
///
/// `Renderer::Wgpu`, never glow. Note that `glow` appearing in the dependency
/// tree does **not** mean the glow renderer is in use: it arrives via
/// `wgpu-hal`'s GLES backend, and [`PolisApp::new`] checks
/// `cc.wgpu_render_state` at runtime and prints the adapter it actually got.
pub fn run(config: config::Config) -> anyhow::Result<()> {
    let repo = config.repo_root.clone();
    launch(config, Mode::Map { repo })
}
