//! Glyph coverage and grid painting, headlessly (ADR-0097).
//!
//! `egui::Context::run_ui` gives a full pass with no window, no adapter and no
//! surface, so both halves of the widget are checkable in CI: whether the font
//! chain can draw what Claude Code draws, and whether a screen turns into
//! shapes.
//!
//! `FullOutput` carries the font-atlas and texture deltas and `epaint` panics if
//! they are dropped unapplied — there is no painter here to apply them to — so
//! every pass ends in `output.textures_delta.clear()`.

use eframe::egui::{self, Rect, Vec2};
use polis_term::emu::Emulator;
use polis_term::{font, widget};

/// One headless pass, with the deltas cleared.
fn pass(ctx: &egui::Context, run: impl FnMut(&mut egui::Ui)) {
    let input = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(
            egui::Pos2::ZERO,
            Vec2::new(1200.0, 800.0),
        )),
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, run);
    output.textures_delta.clear();
}

/// A context with the terminal family installed and one pass behind it, which
/// is what font metrics need.
fn ready() -> (egui::Context, font::Installed) {
    let ctx = egui::Context::default();
    pass(&ctx, |_| {});
    let installed = font::install(&ctx);
    pass(&ctx, |_| {});
    (ctx, installed)
}

/// The claim that justifies loading a font from the operating system at all:
/// eframe's four bundled fonts do **not** cover what Claude Code draws.
///
/// If this ever starts failing, eframe has grown the coverage and
/// `font::install`'s symbol-font lookup can be reconsidered — so it asserts the
/// gap rather than assuming it.
#[test]
fn the_bundled_fonts_alone_do_not_cover_what_claude_code_draws() {
    let ctx = egui::Context::default();
    pass(&ctx, |_| {});
    let monospace = egui::FontId::monospace(14.0);
    let missing: Vec<char> = font::coverage_of(&ctx, &monospace)
        .into_iter()
        .filter(|(_, _, ok)| !ok)
        .map(|(glyph, _, _)| glyph)
        .collect();
    assert!(
        !missing.is_empty(),
        "eframe's bundled fonts now cover every glyph; the system font fallback \
         may no longer be needed"
    );
    // The tool-result elbow leads every tool line, so it is the one whose
    // absence is most visible.
    assert!(
        missing.contains(&'\u{23bf}'),
        "expected the tool-result elbow to be missing; missing set was {missing:?}"
    );
    eprintln!(
        "bundled fonts miss {}/{}: {:?}",
        missing.len(),
        font::CLAUDE_GLYPHS.len(),
        missing
    );
}

/// And the fix. On Windows this must be total; elsewhere the platform may not
/// ship an equivalent, so the requirement is that `install` degrades rather
/// than panics and that the report tells the truth.
#[test]
fn the_system_symbol_font_closes_the_gap() {
    let (ctx, installed) = ready();
    let report = font::coverage(&ctx, 14.0);
    let missing: Vec<char> = report
        .iter()
        .filter(|(_, _, ok)| !ok)
        .map(|(glyph, _, _)| *glyph)
        .collect();
    let line = font::coverage_line(&ctx, 14.0, &installed);
    eprintln!("{line}");

    if cfg!(windows) {
        assert!(
            !installed.symbol_fonts.is_empty(),
            "Windows has shipped seguisym.ttf since 7: {:?}",
            installed.note
        );
        assert!(missing.is_empty(), "still missing {missing:?} — {line}");
    } else {
        // CI's ubuntu leg asserts that installing degrades without panicking,
        // not that coverage holds. `polis doctor` reports the truth.
        assert!(line.starts_with("terminal glyphs"), "{line}");
    }
}

/// A screen has to become shapes, and the run count has to stay in the hundreds
/// rather than the thousands — one galley per cell would be 5 400 a frame.
#[test]
fn a_screen_of_claude_code_paints_as_a_few_hundred_runs() {
    let (ctx, _) = ready();
    let mut emulator = Emulator::new(45, 120);
    // A plausible screenful: a banner, a rule, some tool lines, some colour.
    emulator.feed("\u{2588}\u{2588}\u{2588} Claude Code v2.1.248\r\n".as_bytes());
    emulator.feed(&"\u{2500}".repeat(120).into_bytes());
    emulator.feed(b"\r\n");
    for line in 0..20 {
        emulator.feed(
            format!(
                "\u{23bf}  \x1b[38;5;{}mRead\x1b[0m src/file{line}.rs (\x1b[1m{line}\x1b[0m lines)\r\n",
                30 + line
            )
            .as_bytes(),
        );
    }
    let screen = emulator.snapshot();

    let mut runs = 0;
    pass(&ctx, |ui| {
        let metrics = widget::Metrics::new(ui.ctx(), 14.0);
        let rect = Rect::from_min_size(
            egui::Pos2::ZERO,
            Vec2::new(metrics.cell.x * 120.0, metrics.cell.y * 45.0),
        );
        runs = widget::paint(
            ui.ctx(),
            ui.painter(),
            rect,
            &screen,
            &metrics,
            widget::PaintOptions {
                focused: true,
                ..widget::PaintOptions::default()
            },
        );
    });

    eprintln!("{runs} galley runs for a 45x120 screen");
    assert!(runs > 0, "the widget drew nothing");
    assert!(
        runs < 900,
        "{runs} runs is per-cell territory; the run merging has stopped working"
    );
}

/// The two numbers a resize is told come from the metrics, not the pixel rect.
#[test]
fn the_grid_size_is_derived_from_the_cell() {
    let (ctx, _) = ready();
    pass(&ctx, |ui| {
        let metrics = widget::Metrics::new(ui.ctx(), 14.0);
        assert!(metrics.cell.x > 1.0 && metrics.cell.y > 1.0, "{metrics:?}");
        let rect = Rect::from_min_size(
            egui::Pos2::ZERO,
            Vec2::new(metrics.cell.x * 80.4, metrics.cell.y * 24.9),
        );
        assert_eq!(
            metrics.grid_for(rect),
            (24, 80),
            "a partial cell is not a cell"
        );

        // A window mid-drag legitimately reports nothing at all.
        let empty = Rect::from_min_size(egui::Pos2::ZERO, Vec2::ZERO);
        assert_eq!(metrics.grid_for(empty), (1, 1), "never zero, never a panic");
    });
}

/// An empty screen must still paint its background, or the dock shows through.
#[test]
fn a_blank_pane_still_paints() {
    let (ctx, _) = ready();
    let emulator = Emulator::new(10, 40);
    let screen = emulator.snapshot();
    let mut shapes = 0;
    let input = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(
            egui::Pos2::ZERO,
            Vec2::new(1200.0, 800.0),
        )),
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| {
        let metrics = widget::Metrics::new(ui.ctx(), 14.0);
        widget::paint(
            ui.ctx(),
            ui.painter(),
            ui.max_rect(),
            &screen,
            &metrics,
            widget::PaintOptions::default(),
        );
    });
    shapes += output.shapes.len();
    output.textures_delta.clear();
    assert!(shapes > 0, "a blank pane drew nothing at all");
}

/// The chain is justified by measurement, not by hopefulness: loading a font
/// that adds no glyph is megabytes of resident memory for nothing.
///
/// Hack plus Segoe UI Symbol is 16/16, which is why `install` stops at the
/// first candidate it finds rather than loading every one.
#[test]
fn hack_plus_the_first_symbol_font_is_the_whole_chain() {
    use eframe::egui::{FontData, FontDefinitions, FontFamily};

    let path = std::path::PathBuf::from(r"C:\Windows\Fonts\seguisym.ttf");
    if !path.is_file() {
        return; // not this platform
    }
    let bytes = std::fs::read(&path).expect("read");

    let ctx = egui::Context::default();
    pass(&ctx, |_| {});
    let mut defs = FontDefinitions::default();
    defs.font_data.insert(
        "Segoe UI Symbol".to_owned(),
        std::sync::Arc::new(FontData::from_owned(bytes)),
    );
    defs.families.insert(
        FontFamily::Name("probe".into()),
        vec!["Hack".to_owned(), "Segoe UI Symbol".to_owned()],
    );
    ctx.set_fonts(defs);
    pass(&ctx, |_| {});

    let id = egui::FontId::new(14.0, FontFamily::Name("probe".into()));
    let missing: Vec<char> = font::coverage_of(&ctx, &id)
        .into_iter()
        .filter(|(_, _, ok)| !ok)
        .map(|(glyph, _, _)| glyph)
        .collect();
    assert!(
        missing.is_empty(),
        "Hack + Segoe UI Symbol should be the whole chain, but misses {missing:?}"
    );

    // And `install` loads exactly one, because one is enough.
    let (_, installed) = ready();
    assert_eq!(
        installed.symbol_fonts.len(),
        1,
        "loaded {:?} when one font covers everything",
        installed.names()
    );
}
