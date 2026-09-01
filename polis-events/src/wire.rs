//! The hook wire format (PRD §4.2 step 2, as corrected by
//! `docs/verified/hook-ipc.md`).
//!
//! One loopback UDP datagram per hook event:
//!
//! ```text
//! offset 0   u32 LE  tag   bits 0..=30 event kind, bit 31 = TRUNCATED
//! offset 4   u32 LE  len   payload bytes in THIS datagram
//! offset 8   ..len   payload (raw hook stdin, unparsed)
//! ```
//!
//! Three things PRD §4.2 left unspecified, each of which is a latent cross-crate
//! bug, are fixed here by decision (ADR-0010):
//!
//! * **Endianness is little.** Same-machine loopback protocol, every target
//!   Polis supports is little-endian, so both ends use `to_le_bytes` /
//!   `from_le_bytes` with no byte swap.
//! * **The truncation flag is bit 31 of the tag.** No version field, no magic
//!   number.
//! * **`len` counts the bytes in *this* datagram**, not the original stdin size.
//!   The original size is deliberately unrecoverable: spending header bytes on it
//!   would serve a case the daemon already handles by backfilling from the JSONL
//!   transcript (PRD §4.4).
//!
//! In place of a magic number, [`WireHeader::decode`] validates
//! `datagram_len >= 8`, `len + 8 == datagram_len`, and `kind <= 19`, which
//! rejects essentially all accidental loopback noise for free.
//!
//! # Never panics
//!
//! The endpoint is unauthenticated: any local process can send it anything
//! (`docs/verified/hook-ipc.md` §1.4). Every function here validates before it
//! slices, and the test module feeds it truncated, oversized, and adversarial
//! buffers.

use std::fmt;

use crate::kind::EventKind;

/// The 8-byte header, exactly as PRD §4.2 step 2 specifies.
pub const HEADER_LEN: usize = 8;

/// Largest payload that still fits one IPv4 UDP datagram with the header.
///
/// PRD §4.2's 256 KiB cap is **unsendable**: measured, the hard limit is 65 507
/// payload bytes and 65 508 fails with `WSAEMSGSIZE`. Because `polis-hook`
/// correctly discards the send error, an over-cap event would vanish silently —
/// and large `PermissionRequest` and `PreToolUse` payloads are exactly the ones
/// most worth seeing. 60 KiB leaves 4 059 bytes of slack for future header
/// growth without a wire break.
pub const MAX_PAYLOAD: usize = 60 * 1024;

/// The largest well-formed datagram: [`HEADER_LEN`] + [`MAX_PAYLOAD`] = 61 448.
pub const MAX_DATAGRAM: usize = HEADER_LEN + MAX_PAYLOAD;

/// [`MAX_PAYLOAD`] as a `u32`, so header construction needs no `as` cast.
const MAX_PAYLOAD_U32: u32 = 60 * 1024;

// Compile-time assertions, not tests: an over-cap datagram must be impossible to
// construct, and a test would only say so after the binary shipped.
// 65 507 payload bytes is the measured hard limit; 65 508 gives WSAEMSGSIZE.
const _: () = assert!(MAX_DATAGRAM <= 65_507);
const _: () = assert!(MAX_DATAGRAM == 61_448);
const _: () = assert!(TAG_TRUNCATED | TAG_KIND_MASK == u32::MAX);
const _: () = assert!(TAG_TRUNCATED & TAG_KIND_MASK == 0);
// Below the OS ephemeral range (measured 49 152+ on Windows), so the port can
// never be handed to an anonymous client socket.
const _: () = assert!(DEFAULT_HOOK_PORT < 49_152);

/// Tag bit 31: stdin exceeded [`MAX_PAYLOAD`] and the payload was cut.
///
/// The daemon treats a truncated event as **notification-only** and backfills
/// detail from the JSONL transcript.
pub const TAG_TRUNCATED: u32 = 0x8000_0000;

/// Tag bits 0..=30: the [`EventKind`].
pub const TAG_KIND_MASK: u32 = 0x7fff_ffff;

/// The daemon's fixed receive port.
///
/// Never ephemeral. Measured, the OS dynamic range on Windows starts at 49 152,
/// so 45 177 can never be handed to an anonymous client socket. Had the daemon
/// used an ephemeral port recorded in the endpoint file, a crash could leave the
/// file pointing at a port the OS later reassigns to an unrelated local process —
/// and hook payloads contain source code.
pub const DEFAULT_HOOK_PORT: u16 = 45_177;

/// Explicit endpoint override, injected into agents the daemon launches itself.
///
/// It cannot be the primary mechanism: it only reaches agents Polis started, and
/// operators start `claude` from their own terminal. The endpoint file is the
/// zero-config path (measured at 17 µs p50, i.e. free against a ~5 ms spawn).
pub const HOOK_ENDPOINT_ENV: &str = "POLIS_HOOK_ENDPOINT";

/// `SO_RCVBUF` the daemon must set on its receive socket.
///
/// Measured: at the 65 536-byte default, a burst of 5 000 × 60 KiB datagrams
/// loses 0.9%. At 8 MiB it loses 0%. PRD §13.1 asks for 500 events/sec with zero
/// drops.
pub const RECV_BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// A decoded wire header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireHeader {
    /// The event kind from the tag's low 31 bits. A **hint**: `hook_event_name`
    /// in the payload is authoritative.
    pub kind: EventKind,
    /// Whether the sender cut the payload at [`MAX_PAYLOAD`].
    pub truncated: bool,
    /// Payload bytes in this datagram.
    pub payload_len: u32,
}

impl WireHeader {
    /// A header for a complete (untruncated) payload.
    pub fn new(kind: EventKind, payload_len: u32) -> Self {
        Self {
            kind,
            truncated: false,
            payload_len,
        }
    }

    /// Serialises the header. Infallible.
    pub fn encode(self) -> [u8; HEADER_LEN] {
        let mut tag = self.kind.as_tag() & TAG_KIND_MASK;
        if self.truncated {
            tag |= TAG_TRUNCATED;
        }
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&tag.to_le_bytes());
        out[4..8].copy_from_slice(&self.payload_len.to_le_bytes());
        out
    }

    /// Parses one received datagram into a header and its payload slice.
    ///
    /// This is the trust boundary for an unauthenticated loopback port. It never
    /// panics, never allocates, and never indexes without checking first.
    ///
    /// An unrecognised kind is **not** an error — it degrades to
    /// [`EventKind::Unknown`], because a newer `polis-hook` tagging a newer
    /// Claude Code event is drift, not corruption.
    pub fn decode(datagram: &[u8]) -> Result<(Self, &[u8]), WireError> {
        if datagram.len() < HEADER_LEN {
            return Err(WireError::TooShort {
                got: datagram.len(),
            });
        }
        if datagram.len() > MAX_DATAGRAM {
            return Err(WireError::TooLong {
                got: datagram.len(),
            });
        }
        // Both slices are exactly 4 bytes because of the length check above.
        let tag = u32::from_le_bytes([datagram[0], datagram[1], datagram[2], datagram[3]]);
        let len = u32::from_le_bytes([datagram[4], datagram[5], datagram[6], datagram[7]]);

        // `as u64` before the add: `len` is attacker-controlled and
        // `len + HEADER_LEN` overflows u32 for len >= 0xFFFF_FFF8.
        let declared = u64::from(len) + HEADER_LEN as u64;
        let actual = datagram.len() as u64;
        if declared != actual {
            return Err(WireError::LengthMismatch { declared, actual });
        }

        let header = Self {
            kind: EventKind::from_tag(tag & TAG_KIND_MASK),
            truncated: tag & TAG_TRUNCATED != 0,
            payload_len: len,
        };
        Ok((header, &datagram[HEADER_LEN..]))
    }
}

/// Builds a complete datagram, truncating and flagging an over-cap payload.
///
/// This mirrors what `polis-hook` does; it exists so tests and the synthetic
/// load generator (PRD §16) can produce real frames without spawning a process.
pub fn encode_frame(kind: EventKind, payload: &[u8]) -> Vec<u8> {
    let truncated = payload.len() > MAX_PAYLOAD;
    let body = if truncated {
        &payload[..MAX_PAYLOAD]
    } else {
        payload
    };
    // `body.len() <= MAX_PAYLOAD` (61 440), so the fallback is unreachable;
    // `try_from` simply avoids an `as` cast on a length.
    let header = WireHeader {
        kind,
        truncated,
        payload_len: u32::try_from(body.len()).unwrap_or(MAX_PAYLOAD_U32),
    };
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(body);
    out
}

/// Why a datagram was rejected. Every variant is a *drop*, never a crash.
///
/// `polis-ingest` counts these into the same status-bar counter as dropped
/// events (PRD §4.5) rather than logging each one: on an unauthenticated port,
/// a noisy neighbour must not be able to make Polis write to disk in a loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    /// Fewer than [`HEADER_LEN`] bytes arrived.
    #[error("datagram is {got} bytes, shorter than the {HEADER_LEN}-byte header")]
    TooShort {
        /// Bytes actually received.
        got: usize,
    },
    /// More than [`MAX_DATAGRAM`] bytes arrived — impossible from `polis-hook`.
    #[error("datagram is {got} bytes, longer than the {MAX_DATAGRAM}-byte maximum")]
    TooLong {
        /// Bytes actually received.
        got: usize,
    },
    /// `payload_len + 8` disagrees with the datagram length. This is the check
    /// that stands in for a magic number.
    #[error("header declares {declared} bytes but the datagram is {actual}")]
    LengthMismatch {
        /// `payload_len + HEADER_LEN`, widened so it cannot overflow.
        declared: u64,
        /// The datagram's real length.
        actual: u64,
    },
}

impl fmt::Display for WireHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}[{} B", self.kind, self.payload_len)?;
        if self.truncated {
            f.write_str(", truncated")?;
        }
        f.write_str("]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_kind_at_every_interesting_length() {
        for kind in EventKind::ALL {
            for len in [0usize, 1, 7, 8, 9, 152, 4096, MAX_PAYLOAD] {
                let payload: Vec<u8> = (0..len)
                    .map(|i| u8::try_from(i % 251).unwrap_or(0))
                    .collect();
                let frame = encode_frame(kind, &payload);
                assert_eq!(frame.len(), HEADER_LEN + len);
                let (h, body) = WireHeader::decode(&frame).expect("own frame must decode");
                assert_eq!(h.kind, kind);
                assert!(!h.truncated);
                assert_eq!(h.payload_len as usize, len);
                assert_eq!(body, &payload[..]);
            }
        }
    }

    #[test]
    fn truncation_boundary_is_exact() {
        // Measured on the wire: 61 440 B stdin -> tag 0x00000009, trunc=0;
        // 61 441 B -> tag 0x80000005, trunc=1. Both datagrams 61 448 bytes.
        let exact = vec![b'x'; MAX_PAYLOAD];
        let (h, body) = {
            let f = encode_frame(EventKind::TaskCompleted, &exact);
            assert_eq!(f.len(), MAX_DATAGRAM);
            let (h, b) = WireHeader::decode(&f).unwrap();
            (h, b.to_vec())
        };
        assert!(!h.truncated);
        assert_eq!(body.len(), MAX_PAYLOAD);

        let over = vec![b'x'; MAX_PAYLOAD + 1];
        let f = encode_frame(EventKind::PermissionRequest, &over);
        assert_eq!(
            f.len(),
            MAX_DATAGRAM,
            "an over-cap payload still fits one datagram"
        );
        let (h, body) = WireHeader::decode(&f).unwrap();
        assert!(h.truncated);
        assert_eq!(h.kind, EventKind::PermissionRequest);
        assert_eq!(body.len(), MAX_PAYLOAD);

        let huge = vec![b'y'; 32 * 1024 * 1024];
        let f = encode_frame(EventKind::PreToolUse, &huge);
        assert_eq!(f.len(), MAX_DATAGRAM);
        assert!(WireHeader::decode(&f).unwrap().0.truncated);
    }

    #[test]
    fn truncated_flag_survives_the_header_round_trip() {
        let h = WireHeader {
            kind: EventKind::Stop,
            truncated: true,
            payload_len: 3,
        };
        let mut frame = h.encode().to_vec();
        frame.extend_from_slice(b"abc");
        assert_eq!(WireHeader::decode(&frame).unwrap().0, h);
    }

    #[test]
    fn rejects_short_datagrams_without_panicking() {
        for n in 0..HEADER_LEN {
            let buf = vec![0u8; n];
            assert_eq!(
                WireHeader::decode(&buf),
                Err(WireError::TooShort { got: n })
            );
        }
    }

    #[test]
    fn rejects_a_length_that_disagrees_with_the_datagram() {
        // Header says 100 payload bytes; only 3 arrived.
        let mut buf = Vec::new();
        buf.extend_from_slice(&5u32.to_le_bytes());
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(b"abc");
        assert!(matches!(
            WireHeader::decode(&buf),
            Err(WireError::LengthMismatch { .. })
        ));

        // Header says 0; 3 arrived. Trailing garbage is a mismatch, not a trim.
        let mut buf = Vec::new();
        buf.extend_from_slice(&5u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(b"abc");
        assert!(matches!(
            WireHeader::decode(&buf),
            Err(WireError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn a_hostile_length_cannot_overflow_the_addition() {
        // `len + HEADER_LEN` wraps to 0 in u32 arithmetic for these values, and
        // a wrapped 0 would compare equal to nothing useful. Widened to u64, it
        // is simply a mismatch.
        for len in [u32::MAX, u32::MAX - 1, u32::MAX - 7, 0xFFFF_FFF8] {
            let mut buf = Vec::new();
            buf.extend_from_slice(&3u32.to_le_bytes());
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(b"payload");
            assert!(matches!(
                WireHeader::decode(&buf),
                Err(WireError::LengthMismatch { .. })
            ));
        }
    }

    #[test]
    fn rejects_an_oversized_datagram() {
        let buf = vec![0u8; MAX_DATAGRAM + 1];
        assert!(matches!(
            WireHeader::decode(&buf),
            Err(WireError::TooLong { .. })
        ));
    }

    #[test]
    fn an_unknown_kind_degrades_rather_than_erroring() {
        for tag in [20u32, 30, TAG_KIND_MASK] {
            let mut buf = Vec::new();
            buf.extend_from_slice(&tag.to_le_bytes());
            buf.extend_from_slice(&2u32.to_le_bytes());
            buf.extend_from_slice(b"hi");
            let (h, body) = WireHeader::decode(&buf).expect("drift is not corruption");
            assert_eq!(h.kind, EventKind::Unknown);
            assert!(!h.truncated);
            assert_eq!(body, b"hi");
        }
        // …and the same tags with the truncation bit set.
        for tag in [(EventKind::MAX_TAG + 1) | TAG_TRUNCATED, u32::MAX] {
            let mut buf = Vec::new();
            buf.extend_from_slice(&tag.to_le_bytes());
            buf.extend_from_slice(&0u32.to_le_bytes());
            let (h, body) = WireHeader::decode(&buf).unwrap();
            assert_eq!(h.kind, EventKind::Unknown);
            assert!(h.truncated);
            assert!(body.is_empty());
        }
    }

    /// A cheap stand-in for `cargo-fuzz` (PRD §16). The property under test is
    /// only "does not panic"; correctness is covered above.
    #[test]
    fn never_panics_on_arbitrary_bytes() {
        // xorshift64*, so the corpus is deterministic and reproducible.
        let mut s: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..20_000 {
            let n = (next() % 80) as usize;
            let buf: Vec<u8> = (0..n).map(|_| (next() & 0xff) as u8).collect();
            let _ = WireHeader::decode(&buf);
        }
        // Plus the shapes a random generator will not hit often enough.
        for buf in [
            vec![],
            vec![0xff; 8],
            vec![0x00; 8],
            b"{\"hook_event_name\":\"Stop\"}".to_vec(),
            b"GET / HTTP/1.1\r\n\r\n".to_vec(),
        ] {
            let _ = WireHeader::decode(&buf);
        }
    }

    #[test]
    fn display_is_readable_for_the_status_bar() {
        let h = WireHeader::new(EventKind::PreToolUse, 152);
        assert_eq!(h.to_string(), "PreToolUse[152 B]");
        let h = WireHeader {
            truncated: true,
            ..h
        };
        assert_eq!(h.to_string(), "PreToolUse[152 B, truncated]");
    }
}
