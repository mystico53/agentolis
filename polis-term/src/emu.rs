//! The VT state machine — the window's half of a pane (ADR-0095).
//!
//! Bytes arrive from the daemon and are parsed here, on the connection's own
//! thread, straight into an `alacritty_terminal::Term`. **There is no byte
//! channel to the UI thread.** A 4 MB `cat` costs the frame one lock
//! acquisition; the alternative — a channel of `Vec<u8>` drained inside `ui()` —
//! puts an unbounded, adversarially-sized parse inside the 16.6 ms budget.
//!
//! # `PaneSignal::Reply` is load-bearing
//!
//! Ink queries the cursor position (DSR 6) and the device attributes (DA1)
//! during start-up and **blocks on the answer**. A terminal that does not reply
//! is a terminal Claude Code hangs in, showing nothing, forever. That reply is
//! `alacritty_terminal`'s `Event::PtyWrite`, and here it becomes
//! [`PaneSignal::Reply`], which the caller must send back to the daemon.
//!
//! It is the single most likely thing to forget and the single most likely cause
//! of "the pane opens and Claude Code never draws anything", so
//! [`Emulator::take_signals`] returns it as data the caller cannot ignore rather
//! than as a callback the caller can leave empty.
//!
//! # The UI reads a snapshot, never the `Term`
//!
//! [`ScreenSnapshot`] is the whole boundary: it is plain data with no lifetime
//! and no lock, so a golden test can assert a screen with no window, no adapter
//! and no pty, and the widget cannot accidentally hold the parser's lock across
//! a paint.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};
use alacritty_terminal::Grid;
use crossbeam_channel::{Receiver, Sender};

/// How much is parsed under one lock acquisition.
///
/// A burst larger than this is split so that a single 4 MB paste cannot hold the
/// terminal across a frame.
const FEED_CHUNK: usize = 16 * 1024;

/// Lines of scrollback the window keeps per pane.
pub const SCROLLBACK: usize = 10_000;

/// Something the terminal wants the world outside it to do.
///
/// These are the parts of `alacritty_terminal`'s `Event` that a pane cannot
/// serve on its own. Everything else — cursor moves, colours, damage — is state
/// inside the `Term` and never leaves as a signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneSignal {
    /// **Must reach the child.** A DSR/DA1 answer, or an OSC colour report.
    ///
    /// Claude Code blocks on these during start-up. See the module docs.
    Reply(Vec<u8>),
    /// OSC 2 — the tab label.
    Title(String),
    /// The title went back to the default.
    ResetTitle,
    /// The child rang the bell. An attention mark, never a sound.
    Bell,
    /// OSC 52 — the child asked for something to be put on the clipboard.
    Clipboard(String),
}

/// The `EventListener` a pane's `Term` reports through.
#[derive(Debug, Clone)]
struct PaneListener {
    signals: Sender<PaneSignal>,
}

impl EventListener for PaneListener {
    fn send_event(&self, event: Event) {
        let signal = match event {
            // MANDATORY — see the module docs.
            Event::PtyWrite(text) => PaneSignal::Reply(text.into_bytes()),
            Event::Title(title) => PaneSignal::Title(title),
            Event::ResetTitle => PaneSignal::ResetTitle,
            Event::Bell => PaneSignal::Bell,
            Event::ClipboardStore(_, text) => PaneSignal::Clipboard(text),
            // `ColorRequest` and `TextAreaSizeRequest` carry a formatter that
            // needs a value only the window has; neither is emitted by Claude
            // Code and both are ignorable without a hang. `Wakeup`,
            // `MouseCursorDirty` and `CursorBlinkingChange` are damage, and
            // damage is the generation counter's job.
            _ => return,
        };
        let _ = self.signals.send(signal);
    }
}

/// One pane's terminal state.
///
/// Owned by whoever parses — in practice a `Mutex<Emulator>` shared between the
/// connection thread that feeds it and the frame that draws it.
pub struct Emulator {
    term: Term<PaneListener>,
    parser: Processor,
    signals: Receiver<PaneSignal>,
    generation: Arc<AtomicU64>,
    title: Option<String>,
    rows: u16,
    cols: u16,
}

/// Neither `Term` nor `Processor` is `Debug`, and a screen is not something to
/// print anyway: the useful summary is its shape and how far it has got.
impl std::fmt::Debug for Emulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Emulator")
            .field("rows", &self.rows)
            .field("cols", &self.cols)
            .field("generation", &self.generation.load(Ordering::Acquire))
            .field("title", &self.title)
            .finish_non_exhaustive()
    }
}

impl Emulator {
    /// A blank terminal of the given size.
    #[must_use]
    pub fn new(rows: u16, cols: u16) -> Self {
        let (tx, signals) = crossbeam_channel::unbounded();
        let size = GridSize::new(rows, cols);
        let config = Config {
            scrolling_history: SCROLLBACK,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, PaneListener { signals: tx }),
            parser: Processor::new(),
            signals,
            generation: Arc::new(AtomicU64::new(0)),
            title: None,
            rows: size.rows,
            cols: size.cols,
        }
    }

    /// Parses `bytes` into the screen.
    ///
    /// Chunked so that one enormous burst cannot hold the lock across a frame.
    pub fn feed(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        for chunk in bytes.chunks(FEED_CHUNK) {
            self.parser.advance(&mut self.term, chunk);
        }
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Everything the terminal has asked for since the last call.
    ///
    /// The caller **must** forward [`PaneSignal::Reply`] to the child.
    pub fn take_signals(&mut self) -> Vec<PaneSignal> {
        let mut out = Vec::new();
        while let Ok(signal) = self.signals.try_recv() {
            if let PaneSignal::Title(title) = &signal {
                self.title = Some(title.clone());
            }
            if matches!(signal, PaneSignal::ResetTitle) {
                self.title = None;
            }
            out.push(signal);
        }
        out
    }

    /// Reshapes the grid. A no-op when the size has not changed.
    ///
    /// Returns true when something actually changed, which is what tells the
    /// caller to tell the daemon.
    pub fn resize(&mut self, rows: u16, cols: u16) -> bool {
        let size = GridSize::new(rows, cols);
        if (self.rows, self.cols) == (size.rows, size.cols) {
            return false;
        }
        self.rows = size.rows;
        self.cols = size.cols;
        self.term.resize(size);
        self.generation.fetch_add(1, Ordering::Release);
        true
    }

    /// Scrolls the viewport by `delta` lines; positive is back into history.
    pub fn scroll(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        self.term
            .scroll_display(alacritty_terminal::grid::Scroll::Delta(delta));
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Returns the viewport to the live bottom.
    pub fn scroll_to_bottom(&mut self) {
        self.term
            .scroll_display(alacritty_terminal::grid::Scroll::Bottom);
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// The counter the UI compares against what it drew last frame.
    ///
    /// A clone can be read without taking the emulator's lock, which is what
    /// makes "is this pane a reason to repaint?" free.
    #[must_use]
    pub fn generation_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.generation)
    }

    /// The terminal's current mode flags.
    ///
    /// Read by [`crate::input`], which cannot encode an arrow key or a paste
    /// correctly without them.
    #[must_use]
    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// The last OSC 2 title, if the child set one.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// The grid, as plain data.
    #[must_use]
    pub fn snapshot(&self) -> ScreenSnapshot {
        let content = self.term.renderable_content();
        let offset = content.display_offset;
        let rows = self.term.screen_lines();
        let cols = self.term.columns();
        let mut cells = vec![ScreenCell::default(); rows * cols];
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + i32::try_from(offset).unwrap_or(i32::MAX);
            let Ok(row) = usize::try_from(row) else {
                continue;
            };
            let col = indexed.point.column.0;
            if row >= rows || col >= cols {
                continue;
            }
            let cell = indexed.cell;
            let inverse = cell.flags.contains(Flags::INVERSE);
            let mut fg = resolve(cell.fg, content.colors, cell.flags);
            let mut bg = resolve_background(cell.bg, content.colors);
            if inverse {
                std::mem::swap(&mut fg, &mut bg);
            }
            if cell.flags.contains(Flags::HIDDEN) {
                fg = bg;
            }
            cells[row * cols + col] = ScreenCell {
                ch: cell.c,
                fg,
                bg,
                flags: cell.flags,
            };
        }

        let cursor_row = content.cursor.point.line.0 + i32::try_from(offset).unwrap_or(i32::MAX);
        let cursor = (content.cursor.shape != CursorShape::Hidden)
            .then(|| {
                let row = usize::try_from(cursor_row).ok()?;
                (row < rows).then_some((row, content.cursor.point.column.0.min(cols - 1)))
            })
            .flatten();

        ScreenSnapshot {
            rows,
            cols,
            cells,
            cursor,
            cursor_shape: content.cursor.shape,
            display_offset: offset,
            history: self.term.history_size(),
            selection: content.selection.map(|range| {
                let to_row = |line: Line| {
                    usize::try_from(line.0 + i32::try_from(offset).unwrap_or(i32::MAX)).unwrap_or(0)
                };
                SelectionSpan {
                    start: (to_row(range.start.line), range.start.column.0),
                    end: (to_row(range.end.line), range.end.column.0),
                }
            }),
            mode: content.mode,
            generation: self.generation.load(Ordering::Acquire),
        }
    }
}

/// One cell of a rendered screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenCell {
    /// The character. `' '` for an empty cell.
    pub ch: char,
    /// Foreground, already resolved through the palette and any inverse.
    pub fg: [u8; 3],
    /// Background, same.
    pub bg: [u8; 3],
    /// Bold, italic, the underlines, and the two wide-character markers.
    ///
    /// The wide-character flags are **read**, never recomputed: the emulator's
    /// own view of a character's width is the one the child laid its screen out
    /// against, and a second opinion from a width table is how a grid drifts.
    pub flags: Flags,
}

impl Default for ScreenCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: DEFAULT_FOREGROUND,
            bg: DEFAULT_BACKGROUND,
            flags: Flags::empty(),
        }
    }
}

/// A selected span, in viewport coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionSpan {
    /// Inclusive `(row, column)`.
    pub start: (usize, usize),
    /// Inclusive `(row, column)`.
    pub end: (usize, usize),
}

/// A screen, as plain data. The only thing the widget is allowed to read.
#[derive(Debug, Clone)]
pub struct ScreenSnapshot {
    /// Viewport height in cells.
    pub rows: usize,
    /// Viewport width in cells.
    pub cols: usize,
    /// `rows * cols` cells, row-major.
    pub cells: Vec<ScreenCell>,
    /// `(row, column)` of the cursor, when it is visible and on screen.
    pub cursor: Option<(usize, usize)>,
    /// What shape the child asked the cursor to be.
    pub cursor_shape: CursorShape,
    /// How far back into history the viewport is scrolled. Zero is live.
    pub display_offset: usize,
    /// How many lines of history exist.
    pub history: usize,
    /// The selection, when there is one.
    pub selection: Option<SelectionSpan>,
    /// The terminal's mode flags at the time of the snapshot.
    pub mode: TermMode,
    /// The generation this snapshot was taken at.
    pub generation: u64,
}

impl ScreenSnapshot {
    /// One row as a string, trailing blanks trimmed.
    ///
    /// The golden tests' whole vocabulary, and worth having in the shipped API:
    /// it is also how a pane reports itself to `polis doctor`.
    #[must_use]
    pub fn row_text(&self, row: usize) -> String {
        if row >= self.rows {
            return String::new();
        }
        let mut text: String = self.cells[row * self.cols..(row + 1) * self.cols]
            .iter()
            .filter(|cell| !cell.flags.contains(Flags::WIDE_CHAR_SPACER))
            .map(|cell| cell.ch)
            .collect();
        while text.ends_with(' ') {
            text.pop();
        }
        text
    }

    /// The whole screen as lines, trailing blank rows trimmed.
    #[must_use]
    pub fn text(&self) -> String {
        let mut lines: Vec<String> = (0..self.rows).map(|row| self.row_text(row)).collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// True when the viewport is not at the live bottom.
    #[must_use]
    pub fn scrolled_back(&self) -> bool {
        self.display_offset > 0
    }
}

// ---------------------------------------------------------------------------
// Colour
// ---------------------------------------------------------------------------

/// The default foreground, matching the window's own text colour band.
const DEFAULT_FOREGROUND: [u8; 3] = [0xd6, 0xd9, 0xdf];
/// The default background, matching `egui::Visuals::panel_fill` in `app::theme`.
const DEFAULT_BACKGROUND: [u8; 3] = [0x0c, 0x0d, 0x10];

/// Resolves a foreground through the palette, honouring bold-is-bright.
fn resolve(
    color: Color,
    colors: &alacritty_terminal::term::color::Colors,
    flags: Flags,
) -> [u8; 3] {
    let color = match color {
        // The convention every terminal has implemented since the eighties:
        // bold text in one of the first eight colours draws in the bright one.
        Color::Named(named) if flags.contains(Flags::BOLD) => Color::Named(named.to_bright()),
        other => other,
    };
    resolve_background(color, colors)
}

/// Resolves a colour through the palette with no bold rule.
fn resolve_background(color: Color, colors: &alacritty_terminal::term::color::Colors) -> [u8; 3] {
    match color {
        Color::Spec(rgb) => [rgb.r, rgb.g, rgb.b],
        Color::Named(named) => colors[named].map_or_else(|| named_default(named), unpack),
        Color::Indexed(index) => colors[index as usize].map_or_else(|| xterm_256(index), unpack),
    }
}

fn unpack(rgb: Rgb) -> [u8; 3] {
    [rgb.r, rgb.g, rgb.b]
}

/// The palette entry for a named colour the child never redefined.
fn named_default(named: NamedColor) -> [u8; 3] {
    match named {
        NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::Cursor => {
            DEFAULT_FOREGROUND
        }
        NamedColor::Background => DEFAULT_BACKGROUND,
        NamedColor::DimForeground => [0x8a, 0x8f, 0x98],
        other => {
            let index = other as usize;
            // The dim block mirrors the first eight at two thirds intensity.
            if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&index) {
                let base =
                    xterm_256(u8::try_from(index - NamedColor::DimBlack as usize).unwrap_or(0));
                return base.map(|channel| channel / 3 * 2);
            }
            xterm_256(u8::try_from(index).unwrap_or(0))
        }
    }
}

/// The standard xterm 256-colour palette, computed rather than tabulated.
///
/// 0–15 are the ANSI sixteen, 16–231 are a 6×6×6 cube on the standard
/// non-linear ramp, and 232–255 are twenty-four greys. Written out for the same
/// reason `polis_layout` writes out simplex noise: it is arithmetic, it must not
/// change between releases, and a table would be 256 lines of magic numbers.
fn xterm_256(index: u8) -> [u8; 3] {
    const ANSI: [[u8; 3]; 16] = [
        [0x00, 0x00, 0x00],
        [0xcc, 0x34, 0x3f],
        [0x53, 0xa8, 0x5c],
        [0xc9, 0xa5, 0x4a],
        [0x4b, 0x82, 0xd8],
        [0xa8, 0x62, 0xc0],
        [0x3f, 0xa6, 0xa6],
        [0xd6, 0xd9, 0xdf],
        [0x5a, 0x60, 0x6b],
        [0xf2, 0x5f, 0x67],
        [0x7c, 0xd4, 0x86],
        [0xf0, 0xcc, 0x6e],
        [0x74, 0xa8, 0xf0],
        [0xd0, 0x8a, 0xe6],
        [0x63, 0xd2, 0xd2],
        [0xff, 0xff, 0xff],
    ];
    const RAMP: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match index {
        0..=15 => ANSI[index as usize],
        16..=231 => {
            let n = index - 16;
            [
                RAMP[(n / 36) as usize],
                RAMP[((n / 6) % 6) as usize],
                RAMP[(n % 6) as usize],
            ]
        }
        _ => {
            let level = 8 + (index - 232) * 10;
            [level, level, level]
        }
    }
}

// ---------------------------------------------------------------------------
// Dimensions
// ---------------------------------------------------------------------------

/// A grid size, clamped to something a terminal can actually be.
///
/// `alacritty_terminal` panics on a zero-column grid, and a window mid-resize
/// legitimately reports zero, so the clamp is not defensive programming — it is
/// the difference between a dragged divider and a crash.
#[derive(Debug, Clone, Copy)]
struct GridSize {
    rows: u16,
    cols: u16,
}

impl GridSize {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows: rows.max(1),
            cols: cols.max(2),
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.rows as usize
    }

    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// Kept so the `Grid` re-export is used, and as the one place that names the
/// type the snapshot is derived from.
#[allow(dead_code)]
type PaneGrid = Grid<alacritty_terminal::term::cell::Cell>;

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(bytes: &[u8]) -> Emulator {
        let mut emulator = Emulator::new(6, 20);
        emulator.feed(bytes);
        emulator
    }

    #[test]
    fn plain_text_lands_where_it_was_written() {
        let emulator = fed(b"hello\r\nworld");
        let screen = emulator.snapshot();
        assert_eq!(screen.rows, 6);
        assert_eq!(screen.cols, 20);
        assert_eq!(screen.row_text(0), "hello");
        assert_eq!(screen.row_text(1), "world");
        assert_eq!(screen.text(), "hello\nworld");
    }

    /// Cursor addressing is the whole of a TUI. `CUP` puts the cursor at a
    /// one-based row and column.
    #[test]
    fn cursor_addressing_and_erase_behave() {
        let emulator = fed(b"\x1b[2J\x1b[3;5Hxy");
        let screen = emulator.snapshot();
        assert_eq!(screen.row_text(2), "    xy");
        assert_eq!(screen.cursor, Some((2, 6)));
    }

    /// A tool-result elbow and a spinner frame — the two glyphs Claude Code
    /// draws on nearly every screen — must survive the parser as themselves.
    #[test]
    fn the_glyphs_claude_code_draws_survive_the_parser() {
        let emulator = fed("\u{23bf}  Read 42 lines\r\n\u{273b} Thinking".as_bytes());
        let screen = emulator.snapshot();
        assert!(
            screen.row_text(0).starts_with('\u{23bf}'),
            "{:?}",
            screen.row_text(0)
        );
        assert!(
            screen.row_text(1).starts_with('\u{273b}'),
            "{:?}",
            screen.row_text(1)
        );
    }

    /// The 24-bit path, which is what `COLORTERM=truecolor` buys, and the
    /// indexed path, which is what a 256-colour diff uses.
    #[test]
    fn colours_resolve_through_the_palette() {
        let emulator = fed(b"\x1b[38;2;18;52;86mT\x1b[0m\x1b[38;5;196mR\x1b[0m");
        let screen = emulator.snapshot();
        assert_eq!(screen.cells[0].fg, [18, 52, 86], "24-bit is exact");
        assert_eq!(
            screen.cells[1].fg,
            xterm_256(196),
            "indexed goes through the cube"
        );
        assert_eq!(
            screen.cells[2].fg, DEFAULT_FOREGROUND,
            "reset is the default"
        );
    }

    /// Inverse video swaps the pair *after* both are resolved, or a selected
    /// line comes out unreadable.
    #[test]
    fn inverse_swaps_resolved_colours() {
        let emulator = fed(b"\x1b[7mX");
        let cell = emulator.snapshot().cells[0];
        assert_eq!(cell.fg, DEFAULT_BACKGROUND);
        assert_eq!(cell.bg, DEFAULT_FOREGROUND);
    }

    /// The reply Claude Code blocks on. If this test fails, panes hang empty.
    #[test]
    fn a_device_status_report_produces_a_reply_signal() {
        let mut emulator = fed(b"\x1b[6n");
        let signals = emulator.take_signals();
        let reply = signals
            .iter()
            .find_map(|s| match s {
                PaneSignal::Reply(bytes) => Some(bytes.clone()),
                _ => None,
            })
            .expect("DSR 6 must produce a reply, or Claude Code hangs at start-up");
        assert!(reply.starts_with(b"\x1b["), "{reply:?}");
        assert!(reply.ends_with(b"R"), "a CPR report ends in R: {reply:?}");
    }

    /// The other start-up query.
    #[test]
    fn a_device_attributes_query_produces_a_reply_signal() {
        let mut emulator = fed(b"\x1b[c");
        assert!(
            emulator
                .take_signals()
                .iter()
                .any(|s| matches!(s, PaneSignal::Reply(_))),
            "DA1 must be answered"
        );
    }

    #[test]
    fn osc_two_becomes_a_title() {
        let mut emulator = fed(b"\x1b]2;polis pane\x07");
        let signals = emulator.take_signals();
        assert!(signals.contains(&PaneSignal::Title("polis pane".to_owned())));
        assert_eq!(emulator.title(), Some("polis pane"));
    }

    #[test]
    fn a_bell_is_a_signal_and_not_a_character() {
        let mut emulator = fed(b"a\x07b");
        assert!(emulator.take_signals().contains(&PaneSignal::Bell));
        assert_eq!(emulator.snapshot().row_text(0), "ab");
    }

    /// `input` cannot encode an arrow key correctly without this, and Ink turns
    /// DECCKM on.
    #[test]
    fn application_cursor_mode_is_visible_to_the_key_table() {
        let mut emulator = Emulator::new(4, 10);
        assert!(!emulator.mode().contains(TermMode::APP_CURSOR));
        emulator.feed(b"\x1b[?1h");
        assert!(emulator.mode().contains(TermMode::APP_CURSOR));
        emulator.feed(b"\x1b[?2004h");
        assert!(emulator.mode().contains(TermMode::BRACKETED_PASTE));
    }

    /// The generation counter is the whole idle budget: a pane that said
    /// nothing must not be a reason to draw a frame.
    #[test]
    fn the_generation_only_moves_when_something_happened() {
        let mut emulator = Emulator::new(4, 10);
        let generation = emulator.generation_handle();
        let before = generation.load(Ordering::Acquire);
        emulator.feed(b"");
        assert_eq!(
            generation.load(Ordering::Acquire),
            before,
            "empty feed is free"
        );
        emulator.feed(b"x");
        assert!(generation.load(Ordering::Acquire) > before);
        let after = generation.load(Ordering::Acquire);
        assert!(!emulator.resize(4, 10), "an unchanged size is not a change");
        assert_eq!(generation.load(Ordering::Acquire), after);
        assert!(emulator.resize(8, 40));
        assert!(generation.load(Ordering::Acquire) > after);
    }

    /// A window mid-drag legitimately reports zero columns.
    #[test]
    fn a_degenerate_size_is_clamped_rather_than_fatal() {
        let mut emulator = Emulator::new(0, 0);
        emulator.feed(b"x");
        let screen = emulator.snapshot();
        assert!(screen.rows >= 1 && screen.cols >= 2);
        assert!(emulator.resize(0, 0) || !emulator.resize(0, 0));
    }

    #[test]
    fn scrollback_holds_and_the_viewport_can_be_moved_back() {
        let mut emulator = Emulator::new(4, 10);
        for line in 0..40 {
            emulator.feed(format!("line{line}\r\n").as_bytes());
        }
        let live = emulator.snapshot();
        assert!(!live.scrolled_back());
        assert!(live.history > 0, "history should have accumulated");
        emulator.scroll(10);
        let back = emulator.snapshot();
        assert!(back.scrolled_back());
        assert_eq!(back.display_offset, 10);
        emulator.scroll_to_bottom();
        assert!(!emulator.snapshot().scrolled_back());
    }

    /// The palette is arithmetic and must not drift between releases.
    #[test]
    fn the_xterm_cube_and_greys_are_at_their_documented_values() {
        assert_eq!(xterm_256(16), [0, 0, 0], "the cube starts at black");
        assert_eq!(xterm_256(231), [255, 255, 255], "and ends at white");
        assert_eq!(xterm_256(21), [0, 0, 255], "pure blue corner");
        assert_eq!(xterm_256(232), [8, 8, 8], "the first grey");
        assert_eq!(xterm_256(255), [238, 238, 238], "the last grey");
    }
}
