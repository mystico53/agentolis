//! `polis-term` — terminal panes, split across the process boundary (PRD §15 M7).
//!
//! Polis runs coding agents. Until this crate existed it ran exactly one, as a
//! foreground child inheriting the real console (`polis_app::run`), and PRD §11's
//! whole thesis is *several*. A pane is how the second one gets a keyboard.
//!
//! # The split, and why it is here rather than in the window
//!
//! The obvious build puts the pty in the window: one process, one lock, no
//! protocol. `docs/roadmap/terminal-integration.md` planned exactly that as M7
//! and deferred the daemon to M8. It is built the other way round — **the pty
//! lives in `polis-sessiond` from the first commit** — for three reasons, in
//! order of weight:
//!
//! 1. **A window that owns agents kills them when it closes.** A GPU driver
//!    reset is not rare on Windows, and it would take every running agent with
//!    it. tmux, herdr and cmux each independently reached the same answer.
//! 2. **The pty master is not blocking-readable on Windows** — measured, see
//!    [`pty`]. It needs a `polling::Poller` loop, and a process whose main
//!    thread belongs to winit has nowhere to put one without a thread per pane
//!    that exists only to work around the shape of the window. A daemon *is*
//!    that loop. The inversion made the hard part easier, not harder.
//! 3. Doing it later means doing it twice.
//!
//! The boundary carries **raw bytes, not rendered screens** ([`proto`]). This is
//! the decision that keeps the daemon cheap: it never parses, so the VT state
//! machine, the grid and the widget all sit on the window's side and the daemon
//! stays a pipe with a memory.
//!
//! ```text
//! polis-sessiond                          polis-app (the window)
//!   pty::PtyHost   ── raw bytes ──▶  client::SessionClient ─▶ emu::Emulator
//!   log::ByteLog                                                  │
//!   the four ingest channels                                 widget::paint
//! ```
//!
//! # Modules
//!
//! | Module | Side | What it owns |
//! |---|---|---|
//! | [`proto`] | both | the newline-delimited JSON wire, and nothing else |
//! | [`transport`] | both | where the daemon listens and how a client proves it may connect |
//! | [`exec`] | daemon | `PATH`, `PATHEXT`, and the `.cmd` that is not an image |
//! | [`pty`] | daemon | one child on one pty, driven by a poller loop |
//! | [`log`] | daemon | the raw byte log that makes reattach possible |
//! | [`emu`] | window | `alacritty_terminal::Term` behind a mutex, plus a generation counter |
//! | [`client`] | window | the connection, its reader thread, and the pane registry it feeds |
//! | [`input`] | window | an `egui::Event` turned into the bytes a terminal expects |
//! | [`widget`] | window | one grid painted into one `Rect` |
//! | [`font`] | window | the terminal family, and the seven glyphs no bundled font has |
//!
//! # Threading
//!
//! Per `polis_app`'s table: named `std::thread` plus `crossbeam-channel`, and no
//! tokio. This crate starts two kinds of thread and no others — one
//! `polis-pty-<pane>` per pane inside the daemon, and one `polis-term-client`
//! per connection inside the window.

pub mod client;
pub mod emu;
pub mod exec;
pub mod log;
pub mod proto;
pub mod pty;
pub mod transport;

#[cfg(feature = "ui")]
pub mod font;
#[cfg(feature = "ui")]
pub mod input;
#[cfg(feature = "ui")]
pub mod widget;

pub use client::SessionClient;
pub use emu::{Emulator, ScreenSnapshot};
pub use proto::{Message, PaneId, PaneInfo, Reply, Request, PROTOCOL};
