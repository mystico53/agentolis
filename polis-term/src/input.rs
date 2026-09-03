//! An `egui::Event` turned into the bytes a terminal expects (ADR-0097).
//!
//! # Ctrl+C is broken by default, and it is a safety property
//!
//! `egui-winit` **never emits** `Event::Key { key: C, modifiers: ctrl }`.
//! Verified by reading `egui-winit-0.36.1/src/lib.rs:1021-1035`: `is_copy_command`
//! pushes `Event::Copy` and **returns**, so the key event is never produced. The
//! same is true of Ctrl+X and Ctrl+V.
//!
//! Ctrl+C is the key that interrupts Claude Code. Untreated, the operator cannot
//! stop a runaway agent from inside Polis — that is a safety property, not a
//! convenience, which is why it is named here and asserted in `tests/keys.rs`.
//!
//! The fix lives one layer up, in `eframe::App::raw_input_hook`, which runs
//! before egui processes a frame's input: when a pane has focus and no
//! selection, `Event::Copy` is rewritten back into `Event::Key { C, ctrl }` —
//! the Windows Terminal rule, copy when there is a selection and interrupt when
//! there is not. [`interrupt_instead_of_copy`] is that rewrite, kept here beside
//! the table it feeds.
//!
//! `Event::Paste` is deliberately left alone: pasting is what Ctrl+V means.
//!
//! # Two things that must read the mode
//!
//! * **Arrows.** Ink turns DECCKM on, and under `TermMode::APP_CURSOR` an arrow
//!   is `ESC O A`, not `ESC [ A`. Send the wrong one and the agent's input box
//!   ignores the key.
//! * **Paste.** Without `TermMode::BRACKETED_PASTE` the brackets must not be
//!   sent; *with* it they must, or a pasted multi-line prompt is submitted one
//!   line at a time — destructive, and obvious the first time it happens.
//!
//! # `Ctrl+Alt` is never a control byte
//!
//! On a German, French or Polish layout **`AltGr` is reported as Ctrl+Alt**, and
//! `AltGr+Q` is how you type `@` — the character every agent prompt containing
//! an email address needs. winit delivers the `@` as [`Event::Text`], correctly,
//! and *also* delivers `Event::Key { Q, ctrl + alt }`. Treating that as Ctrl+Q
//! would send `@` followed by `\x11`, which an agent's input box reads as a
//! command.
//!
//! So the combination produces nothing here, which is also what makes it safe
//! for `polis_app::panes::RESERVED` to use `Ctrl+Alt` for the dock's own chords.
//!
//! # Shift+Enter, for free
//!
//! `\x1b\r` is what `/terminal-setup` installs into a terminal's key bindings by
//! hand. A pane has it from the first keystroke with no setup step, which no
//! external Windows terminal gives you.

use alacritty_terminal::term::TermMode;
use eframe::egui::{Event, Key, Modifiers};

/// The bytes `event` should send to the child, if any.
///
/// `None` means "not for the terminal": either egui will describe the same
/// keystroke again as [`Event::Text`], or the chord belongs to Polis.
#[must_use]
pub fn to_bytes(event: &Event, mode: TermMode) -> Option<Vec<u8>> {
    match event {
        // Already composed by winit, IME and dead keys included. **Never**
        // reconstruct a printable from `Key`: that is how a German keyboard
        // loses its umlauts.
        Event::Text(text) if !text.is_empty() => Some(text.as_bytes().to_vec()),
        Event::Paste(text) => Some(paste(text, mode)),
        Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => key_bytes(*key, *modifiers, mode),
        _ => None,
    }
}

/// A paste, bracketed when — and only when — the child asked for it.
///
/// The payload's own `ESC [ 2 0 1 ~` is stripped, so a hostile clipboard cannot
/// close the bracket early and have the rest of itself run as keystrokes.
#[must_use]
pub fn paste(text: &str, mode: TermMode) -> Vec<u8> {
    // A terminal's Enter is `\r`. A clipboard's is `\n`, and `\r\n` on Windows.
    let body = text.replace("\r\n", "\r").replace('\n', "\r");
    if !mode.contains(TermMode::BRACKETED_PASTE) {
        return body.into_bytes();
    }
    let body = body.replace("\x1b[201~", "");
    let mut out = Vec::with_capacity(body.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// Rewrites egui's clipboard events back into the keys they came from.
///
/// Call from `eframe::App::raw_input_hook` when a pane has focus **and has no
/// selection**. With a selection, leave them alone and let the copy happen —
/// that is the rule every terminal on Windows follows.
pub fn interrupt_instead_of_copy(events: &mut [Event]) {
    for event in events {
        let key = match event {
            Event::Copy => Key::C,
            Event::Cut => Key::X,
            _ => continue,
        };
        *event = Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        };
    }
}

/// The table.
#[allow(clippy::too_many_lines)]
fn key_bytes(key: Key, modifiers: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    let ctrl = modifiers.ctrl || modifiers.mac_cmd;
    let alt = modifiers.alt;
    let shift = modifiers.shift;
    let app = mode.contains(TermMode::APP_CURSOR);

    // Scrollback is the window's, not the child's.
    if shift && matches!(key, Key::PageUp | Key::PageDown) {
        return None;
    }
    // AltGr, or one of the dock's reserved chords. Never a control byte —
    // see the module docs; getting this wrong corrupts every `@` a German
    // keyboard types.
    if ctrl && alt {
        return None;
    }

    let bytes = match key {
        Key::Enter => {
            if shift || alt {
                // What `/terminal-setup` installs by hand. See the module docs.
                b"\x1b\r".to_vec()
            } else {
                b"\r".to_vec()
            }
        }
        Key::Tab => {
            if shift {
                // CBT. Claude Code cycles permission modes with this, so it is
                // very visible when wrong.
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        // DEL, not BS: every Unix terminal since the VT220 sends 0x7f here, and
        // sending 0x08 makes an agent's input box delete forwards.
        Key::Backspace => match (ctrl, alt) {
            (true, _) => b"\x17".to_vec(),
            (false, true) => b"\x1b\x7f".to_vec(),
            (false, false) => b"\x7f".to_vec(),
        },
        Key::Escape => b"\x1b".to_vec(),
        Key::ArrowUp => cursor(b'A', modifiers, app),
        Key::ArrowDown => cursor(b'B', modifiers, app),
        Key::ArrowRight => cursor(b'C', modifiers, app),
        Key::ArrowLeft => cursor(b'D', modifiers, app),
        Key::Home => cursor(b'H', modifiers, app),
        Key::End => cursor(b'F', modifiers, app),
        Key::Insert => tilde(2, modifiers),
        Key::Delete => tilde(3, modifiers),
        Key::PageUp => tilde(5, modifiers),
        Key::PageDown => tilde(6, modifiers),
        Key::F1 => function(b'P', modifiers),
        Key::F2 => function(b'Q', modifiers),
        Key::F3 => function(b'R', modifiers),
        Key::F4 => function(b'S', modifiers),
        Key::F5 => tilde(15, modifiers),
        Key::F6 => tilde(17, modifiers),
        Key::F7 => tilde(18, modifiers),
        Key::F8 => tilde(19, modifiers),
        Key::F9 => tilde(20, modifiers),
        Key::F10 => tilde(21, modifiers),
        Key::F11 => tilde(23, modifiers),
        Key::F12 => tilde(24, modifiers),
        Key::Space if ctrl => vec![0],
        // The general rule, and the one that carries Ctrl+C.
        key if ctrl => control(key)?,
        // `ESC` then the character. egui also emits `Event::Text` for an
        // Alt chord on some layouts; the escape is what a terminal expects and
        // the duplicate text is what the `alt` guard in `to_bytes` avoids by
        // only ever reaching here for a key event.
        key if alt => {
            let c = printable(key)?;
            let c = if shift { c.to_ascii_uppercase() } else { c };
            vec![0x1b, c]
        }
        // Everything else printable arrives as `Event::Text` instead.
        _ => return None,
    };
    Some(bytes)
}

/// `ESC [ A` normally, `ESC O A` under DECCKM, `ESC [ 1 ; m A` with modifiers.
fn cursor(final_byte: u8, modifiers: Modifiers, app: bool) -> Vec<u8> {
    if let Some(param) = modifier_param(modifiers) {
        return format!("\x1b[1;{param}{}", final_byte as char).into_bytes();
    }
    if app {
        vec![0x1b, b'O', final_byte]
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

/// `ESC [ n ~`, with `ESC [ n ; m ~` when a modifier is held.
fn tilde(number: u8, modifiers: Modifiers) -> Vec<u8> {
    match modifier_param(modifiers) {
        Some(param) => format!("\x1b[{number};{param}~").into_bytes(),
        None => format!("\x1b[{number}~").into_bytes(),
    }
}

/// `ESC O P` for F1–F4, `ESC [ 1 ; m P` when modified.
fn function(final_byte: u8, modifiers: Modifiers) -> Vec<u8> {
    match modifier_param(modifiers) {
        Some(param) => format!("\x1b[1;{param}{}", final_byte as char).into_bytes(),
        None => vec![0x1b, b'O', final_byte],
    }
}

/// The CSI modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
fn modifier_param(modifiers: Modifiers) -> Option<u8> {
    let mut bits = 0;
    if modifiers.shift {
        bits |= 1;
    }
    if modifiers.alt {
        bits |= 2;
    }
    if modifiers.ctrl || modifiers.mac_cmd {
        bits |= 4;
    }
    (bits != 0).then_some(bits + 1)
}

/// Ctrl+key, by the rule every terminal implements: the letter's position in the
/// alphabet, which is its uppercase code minus `@`.
fn control(key: Key) -> Option<Vec<u8>> {
    let byte = match key {
        Key::Space => 0x00,
        Key::OpenBracket => 0x1b,
        Key::Backslash => 0x1c,
        Key::CloseBracket => 0x1d,
        // US-ASCII unit separator: Ctrl+/ and Ctrl+_ are the same key on most
        // layouts, and both are "undo" to a readline-shaped input box.
        Key::Slash | Key::Questionmark | Key::Minus => 0x1f,
        key => {
            let letter = printable(key)?;
            if !letter.is_ascii_alphabetic() {
                return None;
            }
            letter.to_ascii_uppercase() - 0x40
        }
    };
    Some(vec![byte])
}

/// The ASCII byte a key would type unmodified, for the two rules that need it.
fn printable(key: Key) -> Option<u8> {
    let name = key.name();
    let bytes = name.as_bytes();
    match key {
        Key::A
        | Key::B
        | Key::C
        | Key::D
        | Key::E
        | Key::F
        | Key::G
        | Key::H
        | Key::I
        | Key::J
        | Key::K
        | Key::L
        | Key::M
        | Key::N
        | Key::O
        | Key::P
        | Key::Q
        | Key::R
        | Key::S
        | Key::T
        | Key::U
        | Key::V
        | Key::W
        | Key::X
        | Key::Y
        | Key::Z => Some(bytes[0].to_ascii_lowercase()),
        Key::Num0 => Some(b'0'),
        Key::Num1 => Some(b'1'),
        Key::Num2 => Some(b'2'),
        Key::Num3 => Some(b'3'),
        Key::Num4 => Some(b'4'),
        Key::Num5 => Some(b'5'),
        Key::Num6 => Some(b'6'),
        Key::Num7 => Some(b'7'),
        Key::Num8 => Some(b'8'),
        Key::Num9 => Some(b'9'),
        Key::Space => Some(b' '),
        Key::Minus => Some(b'-'),
        Key::Equals => Some(b'='),
        Key::OpenBracket => Some(b'['),
        Key::CloseBracket => Some(b']'),
        Key::Backslash => Some(b'\\'),
        Key::Semicolon => Some(b';'),
        Key::Quote => Some(b'\''),
        Key::Comma => Some(b','),
        Key::Period => Some(b'.'),
        Key::Slash => Some(b'/'),
        Key::Backtick => Some(b'`'),
        _ => None,
    }
}
