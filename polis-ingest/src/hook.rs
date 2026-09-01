//! Channel B — the hook datagram listener (PRD §4.2).
//!
//! The daemon side of the wire format in [`polis_events::wire`]. Every
//! obligation below is measured in `docs/verified/hook-ipc.md`:
//!
//! * **Bind `127.0.0.1:45177` exclusively.** The bind *is* the singleton lock —
//!   a second daemon gets `AddrInUse`. It must then exit with a clear message
//!   rather than binding elsewhere: silently taking another port produces a
//!   half-populated map and an operator who cannot tell which window is lying.
//! * **Raise `SO_RCVBUF` to 8 MiB.** At the 65 536-byte default a burst of
//!   5 000 × 60 KiB datagrams loses 0.9%; at 8 MiB it loses 0%. PRD §13.1 asks
//!   for 500 events/sec with zero drops.
//! * **Set `SIO_UDP_CONNRESET` to 0** on Windows, as defence in depth.
//! * **Validate before trusting**: `len + 8 == datagram_len` and `kind <= 19`.
//!   The port is unauthenticated; any local process can send anything. The blast
//!   radius is a visualisation drawing something untrue, which is bounded — but
//!   only if the decoder never panics and never allocates on attacker input.
//! * **The argv tag is a hint.** `hook_event_name` in the payload is
//!   authoritative.
//! * **A set truncation bit is notification-only.** Backfill the detail from the
//!   JSONL transcript (PRD §4.4).

use std::net::SocketAddrV4;
use std::path::PathBuf;

use polis_events::{HookEvent, WireError};

use crate::bus::EventSink;

/// The hook listener thread.
#[derive(Debug)]
pub struct HookListener {
    _private: (),
}

impl HookListener {
    /// Binds and starts receiving.
    ///
    /// `AddrInUse` here means another Polis is already running. Unlike every
    /// other channel failure this one is **not** degradable: two daemons would
    /// each see roughly half the events and neither would say so.
    pub fn start(addr: SocketAddrV4, sink: EventSink) -> Result<Self, std::io::Error> {
        let _ = (addr, sink);
        todo!("PRD §4.2 — bind, raise SO_RCVBUF, clear SIO_UDP_CONNRESET, spawn the loop")
    }

    /// Writes the endpoint file through which hooks discover the port.
    ///
    /// Atomic (temp + rename) so a hook cannot read a half-written line, and
    /// deleted best-effort on clean shutdown so later hooks fall back to the
    /// compiled-in 45177 — which is exactly where the next daemon binds. A stale
    /// file needs no cleanup: sending to a closed port is measured as `Ok(n)`,
    /// exit 0, and marginally *cheaper* than the live case.
    pub fn publish_endpoint(&self) -> Result<PathBuf, std::io::Error> {
        todo!("docs/verified/hook-ipc.md §5 — %LOCALAPPDATA%/polis/endpoint, or XDG")
    }
}

/// Decodes one received datagram.
///
/// Split out from the receive loop so PRD §16's hook-safety matrix and a
/// `cargo-fuzz` target can drive it directly without a socket.
pub fn decode_datagram(datagram: &[u8]) -> Result<HookEvent, WireError> {
    let _ = datagram;
    todo!("PRD §4.2 — WireHeader::decode, then HookPayload::parse; a bad body is a drop")
}
