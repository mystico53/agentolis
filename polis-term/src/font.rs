//! The terminal font family, and the seven glyphs no bundled font has (ADR-0097).
//!
//! Claude Code draws a small set of characters on nearly every screen: the
//! tool-result elbow `⎿` leads every tool line, and the spinner cycles through
//! `✻ ✽ ✢`. Untreated, every one of them is a replacement character — which
//! looks like a bug in Polis, and is one, in Polis's font chain.
//!
//! The fix costs zero bytes of binary and raises no licensing question: the
//! operating system already ships a font that covers them, and Polis *reads* it
//! rather than redistributing it. On Windows that is `seguisym.ttf`, Segoe UI
//! Symbol, present on every Windows since 7.
//!
//! # Measured here, against the real font chain
//!
//! `docs/roadmap/terminal-integration.md` reached these numbers by parsing `cmap`
//! tables directly. `tests/glyphs.rs` re-measures them through the renderer that
//! will actually draw them, and agrees:
//!
//! | Chain | Covers |
//! |---|---|
//! | eframe's four bundled fonts | 8 of 16 — box-drawing and blocks yes, `⎿ ✻ ✽ ✢ ✓ ✗` and braille no |
//! | plus `seguisym.ttf` | **16 of 16** |
//!
//! Cascadia Mono and Consolas are in [`candidates`] only as understudies for a
//! machine without Segoe UI Symbol: measured, they add **nothing** on top of it,
//! so loading them as well would be megabytes of resident memory for no glyph.
//!
//! Bundling instead would be strictly worse. Cascadia Mono is the obvious
//! candidate — 363 KiB, SIL OFL — and it does not have `⎿`, `✻` or `✗` either.
//!
//! # The oracle is not `Fonts::has_glyph`
//!
//! This is the trap in the way of checking any of the above, and it cost a wrong
//! diagnosis before it was found. `epaint 0.36.1` implements `has_glyph` as
//!
//! ```text
//! self.resolve_face(c) != self.cached_family.replacement_face_key
//! ```
//!
//! — "is this character served by a different face than `U+FFFD` is?" That is a
//! **false negative for every glyph that lives in the same face as the
//! replacement character**, which is normally the first font in the family.
//! Asked about a single-font family it reports that nothing at all is covered.
//! epaint's own source carries a `TODO` beside a narrower version of the bug.
//!
//! Measured that way, eframe's bundled chain appears to be missing `─ │ ╭ █ ░ ▶`
//! as well. It is not — they are in Hack, and Hack is the face `U+FFFD` resolves
//! to. Believing that reading is how a font fix gets designed for a problem that
//! does not exist, which is exactly what happened here before [`coverage`] was
//! rewritten to lay the character out and compare the atlas rectangle it got
//! against the one `U+FFFD` gets.
//!
//! # A missing symbol font is not an error
//!
//! On Linux none of the candidates is guaranteed present, so [`install`]
//! degrades to replacement characters and **says so** rather than failing.
//! [`coverage_line`] turns a cosmetic mystery — "why is every tool line a box?" —
//! into a one-line diagnosis, and is meant to be printed by `polis doctor`.

use std::path::PathBuf;

use eframe::egui::{self, FontData, FontDefinitions, FontFamily, FontId};

/// The family a pane's grid is drawn in.
pub const FAMILY: &str = "polis-term";

/// Characters Claude Code draws, and what each one is.
///
/// Byte-scanned out of `claude.exe` (227 MB) in UTF-8, UTF-16LE and `\uXXXX`
/// forms — this is a list of what the program *contains*, not a guess about
/// what a terminal might need. The comment on each is why its absence would be
/// noticed.
pub const CLAUDE_GLYPHS: &[(char, &str)] = &[
    ('\u{23bf}', "the tool-result elbow, on every tool line"),
    ('\u{273b}', "a spinner frame"),
    ('\u{273d}', "a spinner frame"),
    ('\u{2722}', "a spinner frame"),
    ('\u{23fa}', "the recording bullet"),
    ('\u{2713}', "a completed step"),
    ('\u{2717}', "a failed step"),
    ('\u{2500}', "the box rule under the input"),
    ('\u{2502}', "a box side"),
    ('\u{256d}', "a rounded box corner"),
    ('\u{2588}', "the banner blocks"),
    ('\u{2591}', "a shaded block"),
    ('\u{25b6}', "the auto-mode chevrons"),
    ('\u{26a0}', "a warning"),
    ('\u{2800}', "a braille cell"),
    ('\u{28ff}', "a braille cell"),
];

/// What [`install`] found.
#[derive(Debug, Clone, Default)]
pub struct Installed {
    /// Every system font that was loaded, in fallback order.
    pub symbol_fonts: Vec<PathBuf>,
    /// What was looked for and not found — for `polis doctor`, not for a panic.
    pub note: Option<String>,
}

impl Installed {
    /// The loaded fonts by filename, for a status line.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.symbol_fonts
            .iter()
            .map(|path| {
                path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                )
            })
            .collect()
    }
}

/// Registers [`FAMILY`] on `ctx`, with the system's symbol font behind Hack.
///
/// Call from the window's theme setup, once. Safe to call again; it replaces the
/// definitions rather than accumulating them.
///
/// The chain is deliberate: **Hack first**, because it is monospace and covers
/// ASCII and the whole of box-drawing at a uniform advance, so the common case
/// stays on the fast path. The symbol font is a *fallback*, reached only for the
/// handful of glyphs Hack lacks.
pub fn install(ctx: &egui::Context) -> Installed {
    let mut definitions = FontDefinitions::default();
    let mut chain = vec!["Hack".to_owned()];
    let mut installed = Installed::default();
    let mut absent = Vec::new();

    for (name, path) in candidates() {
        // One is enough, measured. The rest of the list is understudies.
        if !installed.symbol_fonts.is_empty() {
            break;
        }
        if !path.is_file() {
            absent.push(path.display().to_string());
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            absent.push(format!("{} (unreadable)", path.display()));
            continue;
        };
        definitions.font_data.insert(
            name.clone(),
            std::sync::Arc::new(FontData::from_owned(bytes)),
        );
        chain.push(name);
        installed.symbol_fonts.push(path);
    }
    if installed.symbol_fonts.is_empty() {
        installed.note = Some(format!(
            "no system symbol font found (looked for {})",
            absent.join(", ")
        ));
    }

    // Emoji last, and only as a fallback: an agent that prints one should get
    // it, but nothing about a terminal grid should reach for it first.
    chain.push("NotoEmoji-Regular".to_owned());
    chain.push("Ubuntu-Light".to_owned());

    definitions
        .families
        .insert(FontFamily::Name(FAMILY.into()), chain);
    ctx.set_fonts(definitions);
    installed
}

/// A [`FontId`] in the terminal family.
#[must_use]
pub fn font_id(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(FAMILY.into()))
}

/// Which of [`CLAUDE_GLYPHS`] this machine can actually draw.
///
/// # Why this does not use `Fonts::has_glyph`
///
/// It cannot. `epaint 0.36.1` implements it as
///
/// ```text
/// self.resolve_face(c) != self.cached_family.replacement_face_key
/// ```
///
/// — "is this character served by a different face than `U+FFFD` is?" That is a
/// **false negative for every glyph living in the same face as the replacement
/// character**, which is normally the first font in the family. Asked about a
/// single-font family it answers "nothing is covered" for every character,
/// including the ones it is about to draw perfectly. epaint's own source carries
/// a `TODO` next to a narrower version of the same bug.
///
/// Measured with `has_glyph`, eframe's bundled chain appears to be missing
/// `─ │ ╭ █ ░ ▶`. It is not; they are in Hack, and Hack is the face `U+FFFD`
/// resolves to. Believing that reading is how a font fix gets designed for a
/// problem that does not exist.
///
/// So coverage is measured by **laying the character out and comparing the atlas
/// rectangle it got against the one `U+FFFD` gets**. A character that renders as
/// the replacement glyph is missing; anything else is present. That is the same
/// question a reader of the screen would ask, and it has no false negatives.
///
/// Requires at least one completed pass, because font metrics are not available
/// until `pixels_per_point` is known.
#[must_use]
pub fn coverage(ctx: &egui::Context, size: f32) -> Vec<(char, &'static str, bool)> {
    coverage_of(ctx, &font_id(size))
}

/// [`coverage`], against any font — used to measure what a chain covers
/// *without* [`install`], which is how the claim in this module's docs is kept
/// honest rather than remembered.
#[must_use]
pub fn coverage_of(ctx: &egui::Context, id: &FontId) -> Vec<(char, &'static str, bool)> {
    ctx.fonts_mut(|fonts| {
        let replacement = atlas_rect(fonts, id, char::REPLACEMENT_CHARACTER);
        CLAUDE_GLYPHS
            .iter()
            .map(|(glyph, why)| {
                let drawn = atlas_rect(fonts, id, *glyph);
                (*glyph, *why, drawn.is_some() && drawn != replacement)
            })
            .collect()
    })
}

/// Where in the font atlas one character's glyph landed.
///
/// `None` for a character that laid out to nothing at all.
fn atlas_rect(
    fonts: &mut eframe::egui::epaint::text::FontsView<'_>,
    id: &FontId,
    c: char,
) -> Option<[u16; 4]> {
    let galley = fonts.layout_no_wrap(c.to_string(), id.clone(), eframe::egui::Color32::WHITE);
    let glyph = galley.rows.first()?.glyphs.first()?;
    let uv = glyph.uv_rect;
    Some([uv.min[0], uv.min[1], uv.max[0], uv.max[1]])
}

/// One line for `polis doctor`.
///
/// Prints `terminal glyphs  16/16 (seguisym.ttf + CascadiaMono.ttf)`, or names
/// exactly which glyphs are missing and what each one is for. That turns a
/// cosmetic mystery — "why is every tool line a box?" — into a diagnosis.
#[must_use]
pub fn coverage_line(ctx: &egui::Context, size: f32, installed: &Installed) -> String {
    let report = coverage(ctx, size);
    let have = report.iter().filter(|(_, _, ok)| *ok).count();
    let names = installed.names();
    let source = if names.is_empty() {
        installed
            .note
            .clone()
            .unwrap_or_else(|| "bundled fonts only".to_owned())
    } else {
        names.join(" + ")
    };
    let mut line = format!("terminal glyphs  {have}/{} ({source})", report.len());
    if have < report.len() {
        let missing: Vec<String> = report
            .iter()
            .filter(|(_, _, ok)| !ok)
            .map(|(glyph, why, _)| format!("U+{:04X} {why}", *glyph as u32))
            .collect();
        line.push_str(
            "
  missing: ",
        );
        line.push_str(&missing.join("; "));
    }
    line
}

/// Where each platform keeps a font that closes the gap in [`CLAUDE_GLYPHS`].
///
/// Order is preference order, and only the first one present is loaded. The
/// entries after the first are understudies, not additions — measured, they add
/// no glyph that Segoe UI Symbol does not already have.
fn candidates() -> Vec<(String, PathBuf)> {
    #[cfg(windows)]
    {
        let root =
            std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let fonts = root.join("Fonts");
        vec![
            // `⎿ ✻ ✽ ✢ ✓ ✗` and all 256 braille cells. Shipped with every
            // Windows since 7.
            ("Segoe UI Symbol".to_owned(), fonts.join("seguisym.ttf")),
            // Box-drawing, the blocks and the shades — which `seguisym` does
            // *not* have, and which Claude Code frames its whole input in.
            ("Cascadia Mono".to_owned(), fonts.join("CascadiaMono.ttf")),
            // The fallback'"'"'s fallback: present since Windows 2000, and it also
            // carries box-drawing and blocks.
            ("Consolas".to_owned(), fonts.join("consola.ttf")),
        ]
    }
    #[cfg(target_os = "macos")]
    {
        vec![
            (
                "Apple Symbols".to_owned(),
                PathBuf::from("/System/Library/Fonts/Apple Symbols.ttf"),
            ),
            (
                "Menlo".to_owned(),
                PathBuf::from("/System/Library/Fonts/Menlo.ttc"),
            ),
        ]
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        ["/usr/share/fonts", "/usr/local/share/fonts"]
            .iter()
            .flat_map(|root| {
                [
                    (
                        format!("Noto Sans Symbols 2 ({root})"),
                        Path::new(root).join("truetype/noto/NotoSansSymbols2-Regular.ttf"),
                    ),
                    (
                        format!("DejaVu Sans Mono ({root})"),
                        Path::new(root).join("truetype/dejavu/DejaVuSansMono.ttf"),
                    ),
                    (
                        format!("DejaVu Sans ({root})"),
                        Path::new(root).join("truetype/dejavu/DejaVuSans.ttf"),
                    ),
                    (
                        format!("Unifont ({root})"),
                        Path::new(root).join("opentype/unifont/unifont.otf"),
                    ),
                ]
            })
            .collect()
    }
}
