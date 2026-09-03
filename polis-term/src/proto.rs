//! The wire between the window and the session daemon (PRD §15 M7, ADR-0098).
//!
//! Newline-delimited JSON, one object per line, in both directions. A client
//! sends [`Call`]s and receives [`Message`]s; replies carry the `id` of the call
//! they answer and events carry none.
//!
//! # Bytes, not screens
//!
//! [`Message::Output`] ships the pty's **raw bytes**, base64'd. The obvious
//! alternative — serialise a styled 45×120 grid at 30 Hz per pane — is the
//! design tmux spent years adding flow control to survive, and tmux's own
//! control mode does not do it either: `%output %pane <data>` is raw, and that
//! is how iTerm2 draws tmux panes as native tabs.
//!
//! Shipping bytes is what lets the parser, the grid and the widget live entirely
//! on the window's side. The daemon never learns what a cursor is.
//!
//! Base64 costs 33 % over the wire, against a pty that produces a few hundred
//! kilobytes in a busy second on a loopback socket. It is the wrong thing to
//! optimise, and JSON cannot hold a `\x1b` any other way.
//!
//! # Versioning
//!
//! [`PROTOCOL`] is bumped for **any** change to these types, and
//! [`Request::Hello`] is the first call on every connection. A mismatch is
//! refused with a message naming both versions rather than papered over — a
//! daemon left running across an upgrade is the expected case, not the exotic
//! one.

use std::path::PathBuf;

use polis_events::SessionId;
use serde::{Deserialize, Serialize};

/// The wire version. Bump on **any** change to the types in this module.
///
/// Checked in [`Request::Hello`] before anything else happens on a connection.
pub const PROTOCOL: u32 = 1;

/// Which pane, within one daemon. Small, dense, and stable for the pane's life.
///
/// tmux's documentation is emphatic that ids beat names, and the sigil in the
/// `Display` impl (`%3`) is borrowed from it directly so that a pane id in a log
/// line cannot be mistaken for an ordinal.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct PaneId(pub u32);

impl std::fmt::Display for PaneId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "%{}", self.0)
    }
}

/// One request, with the id its reply will carry.
///
/// Ids are the client's to allocate and need only be unique on one connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Call {
    /// Echoed back in [`Message::Reply`] or [`Message::Failed`].
    pub id: u64,
    /// What is being asked.
    pub request: Request,
}

/// Client to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Request {
    /// Always first. Proves the client may connect, and settles version skew.
    Hello {
        /// The client's [`PROTOCOL`].
        protocol: u32,
        /// Free text for the daemon's log — `"polis-app 0.1.0"`.
        client: String,
        /// The secret from [`crate::transport::Endpoint`].
        token: String,
    },
    /// Every pane the daemon holds, live or exited.
    List,
    /// Start a child on a new pty.
    Open(Box<OpenPane>),
    /// Keystrokes, or anything else headed for the child's stdin.
    Write {
        /// Which pane.
        pane: PaneId,
        /// The bytes, exactly as the child should see them.
        #[serde(with = "bytes_base64")]
        data: Vec<u8>,
    },
    /// Tell the pty its new size. Ignored when it has not changed.
    Resize {
        /// Which pane.
        pane: PaneId,
        /// Rows.
        rows: u16,
        /// Columns.
        cols: u16,
    },
    /// Start receiving this pane's output, from `since` bytes into its log.
    ///
    /// `since: 0` replays the pane from its first byte, which is what a window
    /// reattaching to a daemon it did not start does. A window that has been
    /// following passes the offset it last saw and gets only what it missed.
    Attach {
        /// Which pane.
        pane: PaneId,
        /// How far into the pane's byte log the client already is.
        since: u64,
    },
    /// Stop receiving this pane's output. The child keeps running.
    Detach {
        /// Which pane.
        pane: PaneId,
    },
    /// End the child and forget the pane.
    Close {
        /// Which pane.
        pane: PaneId,
    },
    /// Close every pane and exit. The one call that is not idempotent.
    Shutdown,
}

/// What [`Request::Open`] needs. Boxed in the enum: it is much the largest arm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenPane {
    /// The program, as typed. Resolved against `PATH` by the daemon, which is
    /// also where the `.cmd` rule lives — a client never learns about `cmd.exe`.
    pub program: String,
    /// Arguments, passed through untouched.
    pub args: Vec<String>,
    /// The child's working directory. The repository, normally.
    pub cwd: PathBuf,
    /// Extra environment on top of the daemon's own.
    ///
    /// `polis_ingest::env::agent_env`'s twelve telemetry variables arrive here,
    /// plus `TERM` and `COLORTERM`, which `ConPTY` does not set and Ink reads.
    pub env: Vec<(String, String)>,
    /// Initial rows.
    pub rows: u16,
    /// Initial columns.
    pub cols: u16,
    /// The session id to give the agent, when it is one Polis can name.
    ///
    /// Polis **issues** this rather than inferring it: `claude --session-id
    /// <uuid>` is not gated on `--print`, and the same uuid then keys the OTLP
    /// resource attribute, the hook payload and the transcript's own filename.
    /// Nothing is timed, nothing is raced (ADR-0096).
    pub session_id: Option<SessionId>,
}

/// Daemon to client. Replies carry an `id`; everything else is unsolicited.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Message {
    /// The answer to a [`Call`].
    Reply {
        /// The call being answered.
        id: u64,
        /// What it produced.
        result: Reply,
    },
    /// The call could not be served. The connection stays up.
    Failed {
        /// The call being answered.
        id: u64,
        /// One sentence, for the status bar.
        error: String,
    },
    /// Raw pty output. The only high-volume message, and the reason for base64.
    Output {
        /// Which pane.
        pane: PaneId,
        /// The offset of the **first** byte of `data` in the pane's byte log.
        ///
        /// A client that reconnects passes `offset + data.len()` as
        /// [`Request::Attach`]'s `since` and misses nothing.
        offset: u64,
        /// The bytes, exactly as the pty produced them.
        #[serde(with = "bytes_base64")]
        data: Vec<u8>,
    },
    /// The child exited. The pane stays, so its last screen and its exit code
    /// stay on the operator's monitor. A pane that vanishes takes the error
    /// message with it, and the error message is why you looked.
    Exit {
        /// Which pane.
        pane: PaneId,
        /// The process exit code, when there was one.
        code: Option<i32>,
    },
    /// A pane was opened — by this client, or by another one.
    Opened {
        /// The new pane.
        info: Box<PaneInfo>,
    },
    /// A pane was closed and forgotten.
    Closed {
        /// Which pane.
        pane: PaneId,
    },
    /// The daemon is going away. Sent before the socket closes, best effort.
    GoingAway {
        /// Why, for the status bar.
        reason: String,
    },
}

/// The answer to a [`Request`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    /// [`Request::Hello`] succeeded.
    Hello {
        /// The daemon's [`PROTOCOL`]. Equal to the client's, or this would have
        /// been a [`Message::Failed`].
        protocol: u32,
        /// The daemon's package version, for the status bar.
        version: String,
        /// The daemon's process id, so `polis doctor` can point at it.
        pid: u32,
    },
    /// [`Request::List`] succeeded.
    Panes {
        /// Every pane, in id order.
        panes: Vec<PaneInfo>,
    },
    /// [`Request::Open`] succeeded.
    Opened {
        /// The pane that now exists.
        info: Box<PaneInfo>,
    },
    /// Everything else succeeded and had nothing to say.
    Done,
}

/// What the daemon knows about one pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    /// Its id.
    pub pane: PaneId,
    /// The session id the agent was given, when it is one Polis could name.
    ///
    /// This is the join between a pane and the map: the same id arrives on the
    /// OTLP resource, in every hook payload, and as the transcript's filename.
    pub session_id: Option<SessionId>,
    /// The program, as typed.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Where the child is working.
    pub cwd: PathBuf,
    /// Current rows.
    pub rows: u16,
    /// Current columns.
    pub cols: u16,
    /// The child's process id, while it has one.
    pub pid: Option<u32>,
    /// The last OSC 2 title, or the program name until one arrives.
    ///
    /// Set by the **client**, which is the only side that parses: the daemon
    /// ships bytes and never learns what a title is. It travels here so that a
    /// second window attaching to the same daemon starts with the tab label the
    /// first one had.
    pub title: String,
    /// How many bytes the pane's log holds — the `since` a fresh attach wants.
    pub bytes: u64,
    /// When the pane opened, in milliseconds since the Unix epoch.
    pub started_unix_ms: u64,
    /// The child's exit code, once it has one.
    pub exit: Option<i32>,
}

impl PaneInfo {
    /// True while the child is still running.
    #[must_use]
    pub fn alive(&self) -> bool {
        self.exit.is_none()
    }
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Serialises one value as a wire line, newline included.
///
/// `serde_json` never emits a bare newline inside a string — it escapes it as
/// `\n` — so a line is a frame, with no length prefix and nothing to desync.
/// `every_byte_survives_the_wire` and `a_payload_full_of_newlines_is_still_one_frame`
/// are that claim, asserted.
pub fn line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut out = serde_json::to_string(value)?;
    out.push('\n');
    Ok(out)
}

/// The `serde(with = …)` that puts arbitrary bytes inside a JSON string.
mod bytes_base64 {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = <std::borrow::Cow<'_, str>>::deserialize(d)?;
        STANDARD
            .decode(text.as_ref())
            .map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Session ids
// ---------------------------------------------------------------------------

/// A v4-shaped session id, unique on this machine.
///
/// Written out rather than taking the `uuid` crate, for the same reason
/// `polis_layout::determinism` writes out simplex noise and
/// `LogicalPath::layout_seed` writes out its own hash (ADR-0050): the
/// requirement is *unique here*, not *unpredictable anywhere*, and `uuid` is
/// presently only a transitive dependency. The bits are the process id, the
/// pane ordinal and the wall clock in nanoseconds, with version 4 and the
/// RFC 4122 variant set — Claude Code validates the shape of `--session-id`,
/// so the shape is load-bearing.
#[must_use]
pub fn fresh_session_id(ordinal: u32) -> SessionId {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let pid = u128::from(std::process::id());
    let mixed = nanos ^ (pid << 96) ^ (u128::from(ordinal) << 64);
    let mut bytes = mixed.to_be_bytes();
    // Version 4 in the high nibble of byte 6, variant 0b10 in the top two bits
    // of byte 8. Every other bit is left as mixed.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut hex = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    SessionId::new(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every byte a pty can produce has to survive the wire, including the ones
    /// that are not text. A `\x1b` that arrives as `?` is a broken screen.
    #[test]
    fn every_byte_survives_the_wire() {
        let data: Vec<u8> = (0..=255).collect();
        let message = Message::Output {
            pane: PaneId(3),
            offset: 4096,
            data: data.clone(),
        };
        let text = line(&message).expect("serialise");
        assert!(text.ends_with('\n'), "a frame is a line");
        assert_eq!(text.matches('\n').count(), 1, "one frame, one newline");
        let back: Message = serde_json::from_str(&text).expect("parse");
        match back {
            Message::Output {
                pane,
                offset,
                data: got,
            } => {
                assert_eq!(pane, PaneId(3));
                assert_eq!(offset, 4096);
                assert_eq!(got, data);
            }
            other => panic!("wrong arm: {other:?}"),
        }
    }

    /// A JSON string cannot carry a raw newline, so no payload can ever split a
    /// frame. This is the whole argument for newline framing.
    #[test]
    fn a_payload_full_of_newlines_is_still_one_frame() {
        let message = Message::Output {
            pane: PaneId(0),
            offset: 0,
            data: b"a\nb\r\nc\n\n\n".to_vec(),
        };
        assert_eq!(line(&message).expect("serialise").matches('\n').count(), 1);
    }

    #[test]
    fn a_call_round_trips_with_its_id() {
        let call = Call {
            id: 42,
            request: Request::Resize {
                pane: PaneId(1),
                rows: 45,
                cols: 120,
            },
        };
        let back: Call = serde_json::from_str(&line(&call).expect("serialise")).expect("parse");
        assert_eq!(back.id, 42);
        assert!(matches!(
            back.request,
            Request::Resize {
                rows: 45,
                cols: 120,
                ..
            }
        ));
    }

    /// Claude Code validates `--session-id`, so the shape is load-bearing:
    /// 8-4-4-4-12 lowercase hex, version nibble 4, variant bits 0b10.
    #[test]
    fn a_fresh_session_id_is_shaped_like_a_v4_uuid() {
        let id = fresh_session_id(0);
        let text = id.as_str();
        let parts: Vec<&str> = text.split('-').collect();
        assert_eq!(parts.len(), 5, "{text}");
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "{text}"
        );
        assert!(
            text.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "{text}"
        );
        assert!(
            !text.chars().any(|c| c.is_ascii_uppercase()),
            "lowercase only: {text}"
        );
        assert_eq!(parts[2].as_bytes()[0], b'4', "version nibble: {text}");
        assert!(
            matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'),
            "variant bits: {text}"
        );
    }

    /// Two panes opened in the same process must not collide, even when the
    /// clock is coarse enough to give them the same nanosecond.
    #[test]
    fn session_ids_differ_between_panes() {
        let ids: std::collections::BTreeSet<String> = (0..64)
            .map(|n| fresh_session_id(n).as_str().to_owned())
            .collect();
        assert_eq!(ids.len(), 64);
    }
}
