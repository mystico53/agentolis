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
//!
//! # The window, in one page
//!
//! * [`camera`] — top-down orthographic, no rotation, no tilt (PRD §12), in
//!   base-map pixel space so the overlay lines up with the texture exactly.
//! * [`basemap`] — `polis-render`'s raster, uploaded once and redrawn only on
//!   layout change (PRD §13), plus every static shape projected the same way and
//!   the hit-test that answers "which building is under the cursor".
//! * [`mapview`] — the five layers of PRD §10.3 in order, and the three
//!   semantic-zoom representations of PRD §12.
//! * [`clouds`] — PRD §10.4's iso-bands, summed on the CPU into one texture.
//! * [`labels`] — the collision and decluttering PRD §13 calls "the one
//!   genuinely hard thing a map engine would have bought you".
//! * [`treeview`] — the linked filesystem view, co-equal with the map.
//! * [`palette`] — every colour, clamped into PRD §10.3's band for its layer.
//! * [`ui`] — the overlay: status bar, status rail, detail panel, transport.
//! * [`session`] — the picker, which is `polis replay`'s first-run experience.
//! * [`explain`] — the four sentences that say what a building is, in one place,
//!   so the terminal screen and the window cannot drift apart.
//!
//! # The front door
//!
//! Two modules exist so that none of the above has to be known before the first
//! useful minute:
//!
//! * [`setup`] — what bare `polis` does on a machine that has never run it
//!   (detect, explain in one screen, open the session picker), plus `polis
//!   connect` and `polis doctor`.
//! * [`mod@run`] — `polis run -- claude`, which launches an agent with the telemetry
//!   environment already set on it, the receiver already listening and the map
//!   already open.
//!
//! # Diagnostic environment variables
//!
//! All three are off unless set, print to stderr, and exist because each one
//! answered a question that cost real time to answer any other way:
//!
//! | Variable | Prints |
//! |---|---|
//! | `POLIS_DEBUG_LAYOUT` | points per pixel, the viewport, and the rectangle the map was given — the DPI question |
//! | `POLIS_DEBUG_FRAMES` | every frame's wall time and *why* it was drawn — the idle-budget question |
//! | `POLIS_DEBUG_PICK` | the pointer, its map coordinate and the building it hit — the hit-test question |
//!
//! `POLIS_EDITOR` is not a diagnostic: it is the documented override for what a
//! click on a building runs. See [`config::default_editor_command`].

pub mod app;
pub mod basemap;
pub mod camera;
pub mod citygen;
pub mod cli;
pub mod clouds;
pub mod commands;
pub mod config;
pub mod explain;
pub mod format;
pub mod labels;
pub mod mapview;
pub mod palette;
pub mod run;
pub mod session;
pub mod setup;
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
