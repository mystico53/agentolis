//! The key table, asserted byte for byte (ADR-0097).
//!
//! Every row of the table in `polis_term::input`'s documentation is here. They
//! are dull assertions and they are the difference between a pane and a pane
//! that nearly works: an arrow key in the wrong encoding is a dead arrow key,
//! and `\x03` that never arrives is an agent that cannot be stopped.

use alacritty_terminal::term::TermMode;
use eframe::egui::{Event, Key, Modifiers};
use polis_term::input;

fn plain() -> Modifiers {
    Modifiers::default()
}

fn ctrl() -> Modifiers {
    Modifiers::CTRL
}

fn shift() -> Modifiers {
    Modifiers::SHIFT
}

fn alt() -> Modifiers {
    Modifiers::ALT
}

fn press(key: Key, modifiers: Modifiers) -> Event {
    Event::Key {
        key,
        physical_key: Some(key),
        pressed: true,
        repeat: false,
        modifiers,
    }
}

fn bytes(key: Key, modifiers: Modifiers, mode: TermMode) -> Vec<u8> {
    input::to_bytes(&press(key, modifiers), mode).unwrap_or_default()
}

fn normal() -> TermMode {
    TermMode::empty()
}

/// The key that stops a runaway agent. If this test fails, the operator has no
/// way to interrupt Claude Code from inside Polis.
#[test]
fn ctrl_c_is_an_interrupt() {
    assert_eq!(bytes(Key::C, ctrl(), normal()), b"\x03");
}

/// egui-winit never emits Ctrl+C as a key event — it turns it into
/// `Event::Copy` and returns. Without this rewrite the assertion above is true
/// of a code path nothing ever reaches.
#[test]
fn a_copy_event_becomes_an_interrupt_when_there_is_no_selection() {
    let mut events = vec![Event::Copy, Event::Cut, Event::Paste("x".to_owned())];
    input::interrupt_instead_of_copy(&mut events);
    assert_eq!(
        input::to_bytes(&events[0], normal()),
        Some(b"\x03".to_vec())
    );
    assert_eq!(
        input::to_bytes(&events[1], normal()),
        Some(b"\x18".to_vec())
    );
    assert!(
        matches!(events[2], Event::Paste(_)),
        "paste is left alone — pasting is what Ctrl+V means"
    );
}

#[test]
fn the_control_letters_follow_the_minus_forty_rule() {
    for (key, want) in [
        (Key::A, 0x01),
        (Key::C, 0x03),
        (Key::D, 0x04),
        (Key::L, 0x0c),
        (Key::R, 0x12),
        (Key::U, 0x15),
        (Key::W, 0x17),
        (Key::Z, 0x1a),
    ] {
        assert_eq!(
            bytes(key, ctrl(), normal()),
            vec![want],
            "ctrl+{key:?} should be {want:#04x}"
        );
    }
    assert_eq!(
        bytes(Key::Space, ctrl(), normal()),
        vec![0x00],
        "ctrl+space is NUL"
    );
}

/// Enter is `\r`, never `\n`. Shift+Enter is what `/terminal-setup` installs by
/// hand, and a pane has it for free.
#[test]
fn enter_and_shift_enter() {
    assert_eq!(bytes(Key::Enter, plain(), normal()), b"\r");
    assert_eq!(bytes(Key::Enter, shift(), normal()), b"\x1b\r");
    assert_eq!(bytes(Key::Enter, alt(), normal()), b"\x1b\r");
}

/// Claude Code cycles permission modes with Shift+Tab. Very visible when wrong.
#[test]
fn tab_and_shift_tab() {
    assert_eq!(bytes(Key::Tab, plain(), normal()), b"\t");
    assert_eq!(bytes(Key::Tab, shift(), normal()), b"\x1b[Z");
}

/// DEL, not BS. Sending 0x08 makes an input box delete forwards.
#[test]
fn backspace_is_delete() {
    assert_eq!(bytes(Key::Backspace, plain(), normal()), b"\x7f");
    assert_eq!(bytes(Key::Backspace, ctrl(), normal()), b"\x17");
    assert_eq!(bytes(Key::Backspace, alt(), normal()), b"\x1b\x7f");
}

#[test]
fn escape_and_delete_and_the_navigation_block() {
    assert_eq!(bytes(Key::Escape, plain(), normal()), b"\x1b");
    assert_eq!(bytes(Key::Insert, plain(), normal()), b"\x1b[2~");
    assert_eq!(bytes(Key::Delete, plain(), normal()), b"\x1b[3~");
    assert_eq!(bytes(Key::PageUp, plain(), normal()), b"\x1b[5~");
    assert_eq!(bytes(Key::PageDown, plain(), normal()), b"\x1b[6~");
}

/// Ink enables DECCKM, so reading the mode is mandatory rather than optional.
#[test]
fn arrows_change_shape_under_application_cursor_mode() {
    for (key, normal_bytes, app_bytes) in [
        (Key::ArrowUp, &b"\x1b[A"[..], &b"\x1bOA"[..]),
        (Key::ArrowDown, b"\x1b[B", b"\x1bOB"),
        (Key::ArrowRight, b"\x1b[C", b"\x1bOC"),
        (Key::ArrowLeft, b"\x1b[D", b"\x1bOD"),
    ] {
        assert_eq!(bytes(key, plain(), normal()), normal_bytes, "{key:?}");
        assert_eq!(
            bytes(key, plain(), TermMode::APP_CURSOR),
            app_bytes,
            "{key:?} under DECCKM"
        );
    }
}

/// A modified arrow is always the CSI form, whatever the cursor mode.
#[test]
fn modified_arrows_carry_the_csi_parameter() {
    assert_eq!(bytes(Key::ArrowUp, ctrl(), normal()), b"\x1b[1;5A");
    assert_eq!(bytes(Key::ArrowUp, alt(), normal()), b"\x1b[1;3A");
    assert_eq!(bytes(Key::ArrowUp, shift(), normal()), b"\x1b[1;2A");
    assert_eq!(
        bytes(Key::ArrowUp, ctrl(), TermMode::APP_CURSOR),
        b"\x1b[1;5A",
        "a modifier always wins over application-cursor mode"
    );
}

#[test]
fn home_and_end_follow_the_same_rule_as_the_arrows() {
    assert_eq!(bytes(Key::Home, plain(), normal()), b"\x1b[H");
    assert_eq!(bytes(Key::End, plain(), normal()), b"\x1b[F");
    assert_eq!(bytes(Key::Home, plain(), TermMode::APP_CURSOR), b"\x1bOH");
    assert_eq!(bytes(Key::End, plain(), TermMode::APP_CURSOR), b"\x1bOF");
}

#[test]
fn the_function_keys() {
    assert_eq!(bytes(Key::F1, plain(), normal()), b"\x1bOP");
    assert_eq!(bytes(Key::F2, plain(), normal()), b"\x1bOQ");
    assert_eq!(bytes(Key::F3, plain(), normal()), b"\x1bOR");
    assert_eq!(bytes(Key::F4, plain(), normal()), b"\x1bOS");
    assert_eq!(bytes(Key::F5, plain(), normal()), b"\x1b[15~");
    assert_eq!(bytes(Key::F6, plain(), normal()), b"\x1b[17~");
    assert_eq!(bytes(Key::F7, plain(), normal()), b"\x1b[18~");
    assert_eq!(bytes(Key::F8, plain(), normal()), b"\x1b[19~");
    assert_eq!(bytes(Key::F9, plain(), normal()), b"\x1b[20~");
    assert_eq!(bytes(Key::F10, plain(), normal()), b"\x1b[21~");
    assert_eq!(bytes(Key::F11, plain(), normal()), b"\x1b[23~");
    assert_eq!(bytes(Key::F12, plain(), normal()), b"\x1b[24~");
}

#[test]
fn alt_prefixes_an_escape() {
    assert_eq!(bytes(Key::B, alt(), normal()), b"\x1bb");
    assert_eq!(
        bytes(Key::B, alt() | shift(), normal()),
        b"\x1bB",
        "shift makes it the capital"
    );
}

/// On a German layout `AltGr+Q` is `@`, and winit reports `AltGr` as Ctrl+Alt.
/// Treating that as Ctrl+Q would send `@` followed by `\x11` — a control byte
/// into an agent's input box — every time somebody typed an email address.
#[test]
fn altgr_is_not_a_control_byte() {
    let altgr = Modifiers {
        ctrl: true,
        alt: true,
        ..Modifiers::default()
    };
    assert!(
        input::to_bytes(&press(Key::Q, altgr), normal()).is_none(),
        "AltGr+Q must produce nothing; the `@` arrives as Event::Text"
    );
    assert_eq!(
        input::to_bytes(&Event::Text("@".to_owned()), normal()),
        Some(b"@".to_vec()),
        "and that is where the character actually comes from"
    );
    // The same rule is what makes Ctrl+Alt safe for the dock's own chords.
    assert!(input::to_bytes(&press(Key::T, altgr), normal()).is_none());
    assert!(input::to_bytes(&press(Key::Num1, altgr), normal()).is_none());
}

/// Scrollback belongs to the window. Forwarding Shift+PageUp would make the
/// scroll chip unreachable and hand the agent a key it does nothing with.
#[test]
fn shift_page_keys_are_kept_by_the_window() {
    assert!(input::to_bytes(&press(Key::PageUp, shift()), normal()).is_none());
    assert!(input::to_bytes(&press(Key::PageDown, shift()), normal()).is_none());
}

/// Printable characters arrive as `Event::Text`, already composed by winit. A
/// key event for one must produce nothing, or every letter is typed twice.
#[test]
fn a_plain_letter_comes_from_text_and_not_from_the_key() {
    assert!(input::to_bytes(&press(Key::A, plain()), normal()).is_none());
    assert!(input::to_bytes(&press(Key::Num7, plain()), normal()).is_none());
    assert_eq!(
        input::to_bytes(&Event::Text("ä".to_owned()), normal()),
        Some("ä".as_bytes().to_vec()),
        "a dead-key composition has to survive as itself"
    );
}

/// Without the mode check a pasted multi-line prompt is submitted line by line,
/// which is destructive and obvious the first time it happens.
#[test]
fn paste_is_bracketed_only_when_the_child_asked_for_it() {
    let text = "first\nsecond";
    assert_eq!(
        input::paste(text, normal()),
        b"first\rsecond",
        "unbracketed, but newlines still become carriage returns"
    );
    assert_eq!(
        input::paste(text, TermMode::BRACKETED_PASTE),
        b"\x1b[200~first\rsecond\x1b[201~"
    );
    assert_eq!(
        input::paste("a\r\nb", TermMode::BRACKETED_PASTE),
        b"\x1b[200~a\rb\x1b[201~",
        "a Windows clipboard's CRLF must not become two returns"
    );
}

/// A clipboard that contains the closing bracket must not be able to end the
/// paste early and have the rest of itself run as keystrokes.
#[test]
fn a_hostile_clipboard_cannot_close_the_bracket_early() {
    let hostile = "safe\x1b[201~rm -rf /\r";
    let out = input::paste(hostile, TermMode::BRACKETED_PASTE);
    let text = String::from_utf8_lossy(&out);
    assert_eq!(text.matches("\x1b[201~").count(), 1, "{text:?}");
    assert!(text.ends_with("\x1b[201~"), "{text:?}");
}

/// A key going up is not a key going down.
#[test]
fn a_release_sends_nothing() {
    let release = Event::Key {
        key: Key::A,
        physical_key: Some(Key::A),
        pressed: false,
        repeat: false,
        modifiers: ctrl(),
    };
    assert!(input::to_bytes(&release, normal()).is_none());
}
