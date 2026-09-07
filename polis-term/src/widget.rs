//! One terminal grid, painted into one `Rect`.
//!
//! # One galley per attribute run, with the pen reset at every run boundary
//!
//! Not one per cell — that is 5 400 layout calls a frame at 45×120. And not one
//! per row either, which looks fine until a glyph resolves to a fallback face
//! with a different advance and the row drifts; a terminal that drifts is a
//! broken terminal.
//!
//! A run breaks when the foreground, the background, or any of the attribute
//! flags change, **and** whenever a character is not exactly one cell wide —
//! those get a single-character galley centred in their own cell. Because every
//! run's origin is computed from its column index rather than from the previous
//! run's width, drift cannot accumulate even when the oracle is wrong.
//!
//! The oracle is memoised per (font, size): ASCII is single-width by
//! construction because Hack is monospace, and everything else is measured once
//! into [`Metrics`], which the caller keeps across frames.
//!
//! # Backgrounds are ours, not `TextFormat`'s
//!
//! epaint sizes a `TextFormat::background` to the glyph. A terminal's background
//! has to tile with no seams, so adjacent same-background cells are merged into
//! one `rect_filled` at exact cell boundaries instead.
//!
//! # The cursor does not blink
//!
//! A blinking cursor is a permanent 2 Hz wake-up, and would be the single
//! largest regression against PRD §13.1's idle budget in the product. It is a
//! filled block when the pane has focus and a hollow one when it does not, which
//! carries the same information without costing a frame a second.

// A grid is arithmetic against a pixel grid: a column index becomes an x, and a
// measured height becomes a row count. The same three allows sit at the top of
// `polis_app::app` for the same reason.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::cell::RefCell;
use std::collections::HashMap;

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::CursorShape;
use eframe::egui::{self, Color32, FontId, Painter, Pos2, Rect, Stroke, Vec2};

use crate::emu::{ScreenCell, ScreenSnapshot};
use crate::font;

/// How far a glyph's advance may differ from the cell before it is given its own
/// galley. One tenth of a cell is well inside a rounding error and well outside
/// a genuinely double-width glyph.
const WIDTH_EPSILON: f32 = 0.1;

/// Cell metrics for one (font size, points-per-pixel), plus the width oracle.
///
/// Kept by the caller across frames: building it costs two font lookups, and the
/// oracle only pays for itself once it has been asked twice.
pub struct Metrics {
    /// The font panes are drawn in.
    pub font: FontId,
    /// One cell, in points. Height is rounded so `n` rows land on pixels.
    pub cell: Vec2,
    single_width: RefCell<HashMap<char, bool>>,
}

impl std::fmt::Debug for Metrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Metrics")
            .field("font", &self.font)
            .field("cell", &self.cell)
            .field("measured", &self.single_width.borrow().len())
            .finish()
    }
}

impl Metrics {
    /// Measures the terminal family at `size`.
    ///
    /// Requires at least one completed pass — font metrics are not available
    /// until `pixels_per_point` is known.
    #[must_use]
    pub fn new(ctx: &egui::Context, size: f32) -> Self {
        let font = font::font_id(size);
        let (width, height) = ctx.fonts_mut(|fonts| {
            // 'M' rather than a space: a space can be narrower than the advance
            // in a font that is only nearly monospace.
            (fonts.glyph_width(&font, 'M'), fonts.row_height(&font))
        });
        Self {
            font,
            cell: Vec2::new(width.max(1.0), height.round().max(1.0)),
            single_width: RefCell::new(HashMap::new()),
        }
    }

    /// How many rows and columns fit in `rect`.
    ///
    /// These two numbers, not the pixel rectangle, are what a resize is told.
    #[must_use]
    pub fn grid_for(&self, rect: Rect) -> (u16, u16) {
        let rows = (rect.height() / self.cell.y).floor().max(1.0);
        let cols = (rect.width() / self.cell.x).floor().max(1.0);
        (
            u16::try_from(rows as u32).unwrap_or(u16::MAX),
            u16::try_from(cols as u32).unwrap_or(u16::MAX),
        )
    }

    /// The width `cols` columns need.
    #[must_use]
    pub fn width_for(&self, cols: u16) -> f32 {
        f32::from(cols) * self.cell.x
    }

    /// Whether `c` advances by exactly one cell, memoised.
    fn is_single_width(&self, ctx: &egui::Context, c: char) -> bool {
        if c.is_ascii() {
            return true;
        }
        if let Some(known) = self.single_width.borrow().get(&c) {
            return *known;
        }
        let width = ctx.fonts_mut(|fonts| fonts.glyph_width(&self.font, c));
        let single = (width - self.cell.x).abs() <= self.cell.x * WIDTH_EPSILON;
        self.single_width.borrow_mut().insert(c, single);
        single
    }
}

/// What the caller wants painted beyond the grid itself.
#[derive(Debug, Clone, Copy)]
pub struct PaintOptions {
    /// A focused pane gets a filled cursor; an unfocused one a hollow outline.
    pub focused: bool,
    /// Painted behind everything, so a partial last row is not a hole.
    pub background: Color32,
    /// The selection wash.
    pub selection: Color32,
}

impl Default for PaintOptions {
    fn default() -> Self {
        Self {
            focused: false,
            background: Color32::from_rgb(0x0c, 0x0d, 0x10),
            selection: Color32::from_rgba_unmultiplied(0x4b, 0x82, 0xd8, 0x55),
        }
    }
}

/// Paints `screen` into `rect`.
///
/// Returns how many glyph runs were laid out, which is the number worth watching
/// when a frame gets slow: it should be a few hundred for a full screen of
/// Claude Code, not a few thousand.
pub fn paint(
    ctx: &egui::Context,
    painter: &Painter,
    rect: Rect,
    screen: &ScreenSnapshot,
    metrics: &Metrics,
    options: PaintOptions,
) -> usize {
    painter.rect_filled(rect, 0.0, options.background);
    let origin = rect.min;
    let cursor = options.focused.then_some(screen.cursor).flatten();
    let mut runs = 0;

    for row in 0..screen.rows {
        let cells = &screen.cells[row * screen.cols..(row + 1) * screen.cols];
        let top = origin.y + row as f32 * metrics.cell.y;
        if top > rect.max.y {
            break;
        }

        paint_backgrounds(painter, origin, row, cells, metrics, cursor);
        paint_selection(painter, origin, row, screen, metrics, options.selection);
        runs += paint_text(ctx, painter, origin, row, cells, metrics, cursor);
    }

    if let Some((row, col)) = screen.cursor {
        if !options.focused && screen.cursor_shape != CursorShape::Hidden {
            let cell = cell_rect(origin, row, col, metrics);
            painter.rect_stroke(
                cell,
                0.0,
                Stroke::new(1.0, colour(screen_fg(screen, row, col))),
                egui::StrokeKind::Inside,
            );
        }
    }
    runs
}

/// Merges adjacent same-background cells into one rectangle each.
fn paint_backgrounds(
    painter: &Painter,
    origin: Pos2,
    row: usize,
    cells: &[ScreenCell],
    metrics: &Metrics,
    cursor: Option<(usize, usize)>,
) {
    let mut start = 0usize;
    let mut current = effective_bg(cells, 0, row, cursor);
    for col in 1..=cells.len() {
        let next = (col < cells.len()).then(|| effective_bg(cells, col, row, cursor));
        if next == Some(current) {
            continue;
        }
        let span = Rect::from_min_max(
            Pos2::new(
                origin.x + start as f32 * metrics.cell.x,
                origin.y + row as f32 * metrics.cell.y,
            ),
            Pos2::new(
                origin.x + col as f32 * metrics.cell.x,
                origin.y + (row + 1) as f32 * metrics.cell.y,
            ),
        );
        painter.rect_filled(span, 0.0, colour(current));
        start = col;
        if let Some(next) = next {
            current = next;
        }
    }
}

/// The background a cell is actually drawn with, cursor included.
fn effective_bg(
    cells: &[ScreenCell],
    col: usize,
    row: usize,
    cursor: Option<(usize, usize)>,
) -> [u8; 3] {
    if cursor == Some((row, col)) {
        return cells[col].fg;
    }
    cells[col].bg
}

fn paint_selection(
    painter: &Painter,
    origin: Pos2,
    row: usize,
    screen: &ScreenSnapshot,
    metrics: &Metrics,
    wash: Color32,
) {
    let Some(span) = screen.selection else { return };
    if row < span.start.0 || row > span.end.0 {
        return;
    }
    let first = if row == span.start.0 { span.start.1 } else { 0 };
    let last = if row == span.end.0 {
        span.end.1.min(screen.cols.saturating_sub(1))
    } else {
        screen.cols.saturating_sub(1)
    };
    if first > last {
        return;
    }
    let rect = Rect::from_min_max(
        Pos2::new(
            origin.x + first as f32 * metrics.cell.x,
            origin.y + row as f32 * metrics.cell.y,
        ),
        Pos2::new(
            origin.x + (last + 1) as f32 * metrics.cell.x,
            origin.y + (row + 1) as f32 * metrics.cell.y,
        ),
    );
    painter.rect_filled(rect, 0.0, wash);
}

/// One galley per attribute run; one per cell for anything not a cell wide.
fn paint_text(
    ctx: &egui::Context,
    painter: &Painter,
    origin: Pos2,
    row: usize,
    cells: &[ScreenCell],
    metrics: &Metrics,
    cursor: Option<(usize, usize)>,
) -> usize {
    let mut runs = 0;
    let mut col = 0usize;
    while col < cells.len() {
        let cell = cells[col];
        // A spacer is the right half of a wide character that was already drawn.
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            col += 1;
            continue;
        }
        if cell.ch == ' '
            && !cell
                .flags
                .intersects(Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
        {
            col += 1;
            continue;
        }

        let start = col;
        let mut text = String::new();
        let odd = !metrics.is_single_width(ctx, cell.ch);
        loop {
            let here = cells[col];
            text.push(here.ch);
            col += 1;
            if odd || col >= cells.len() {
                break;
            }
            let next = cells[col];
            if !same_run(cell, next, row, col, cursor)
                || next.ch == ' '
                || next.flags.contains(Flags::WIDE_CHAR_SPACER)
                || !metrics.is_single_width(ctx, next.ch)
            {
                break;
            }
        }

        let fg = if cursor == Some((row, start)) {
            cell.bg
        } else {
            cell.fg
        };
        let galley = painter.layout_no_wrap(text, metrics.font.clone(), colour(fg));
        let at = Pos2::new(
            origin.x + start as f32 * metrics.cell.x,
            origin.y + row as f32 * metrics.cell.y,
        );
        painter.galley(at, galley, colour(fg));
        runs += 1;

        if cell.flags.intersects(Flags::ALL_UNDERLINES) {
            let y = origin.y + (row + 1) as f32 * metrics.cell.y - 1.0;
            painter.line_segment(
                [
                    Pos2::new(at.x, y),
                    Pos2::new(origin.x + col as f32 * metrics.cell.x, y),
                ],
                Stroke::new(1.0, colour(fg)),
            );
        }
        if cell.flags.contains(Flags::STRIKEOUT) {
            let y = origin.y + (row as f32 + 0.5) * metrics.cell.y;
            painter.line_segment(
                [
                    Pos2::new(at.x, y),
                    Pos2::new(origin.x + col as f32 * metrics.cell.x, y),
                ],
                Stroke::new(1.0, colour(fg)),
            );
        }
    }
    runs
}

/// Whether two cells may share a galley.
fn same_run(
    a: ScreenCell,
    b: ScreenCell,
    row: usize,
    col: usize,
    cursor: Option<(usize, usize)>,
) -> bool {
    // The cursor cell is drawn inverted, so it is always its own run.
    cursor != Some((row, col))
        && a.fg == b.fg
        && a.bg == b.bg
        && a.flags.difference(Flags::WRAPLINE) == b.flags.difference(Flags::WRAPLINE)
}

fn cell_rect(origin: Pos2, row: usize, col: usize, metrics: &Metrics) -> Rect {
    Rect::from_min_size(
        Pos2::new(
            origin.x + col as f32 * metrics.cell.x,
            origin.y + row as f32 * metrics.cell.y,
        ),
        metrics.cell,
    )
}

fn screen_fg(screen: &ScreenSnapshot, row: usize, col: usize) -> [u8; 3] {
    screen
        .cells
        .get(row * screen.cols + col)
        .map_or([0xd6, 0xd9, 0xdf], |cell| cell.fg)
}

fn colour(rgb: [u8; 3]) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}
