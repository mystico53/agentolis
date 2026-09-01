//! polis-hook — the Polis hook transport binary (PRD §4.2).
//!
//! Contract:
//!   * never blocks the agent
//!   * never exits non-zero, for any reason, ever
//!   * an absent daemon is a normal condition, not an error
//!   * no dependencies beyond `std`, on every platform
//!   * no JSON parsing: the payload is shipped as opaque bytes
//!
//! Wire format — one UDP datagram to a loopback address:
//!
//!   offset 0  u32 LE  tag   bits 0..=30 event kind, bit 31 = TRUNCATED
//!   offset 4  u32 LE  len   payload bytes in THIS datagram
//!   offset 8  ..len   payload (raw hook stdin, unparsed)
//!
//! Little-endian is fixed by decision: this is a same-machine loopback
//! protocol, every target Polis supports is little-endian, and LE lets both
//! ends use `to_le_bytes`/`from_le_bytes` with no byte swap.
//!
//! See docs/verified/hook-ipc.md for the measurements that fix every constant
//! and for the daemon-side receiver requirements.

use std::io::Read;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

/// 8-byte header, exactly as PRD §4.2 step 2 specifies.
const HEADER_LEN: usize = 8;

/// Largest payload that still fits one IPv4 UDP datagram with the header.
/// Measured hard limit on this machine: 65507 bytes of payload, 65508 ->
/// WSAEMSGSIZE. 60 KiB + 8 = 61448 leaves comfortable slack.
///
/// This supersedes PRD §4.2's 256 KiB stdin cap for the *send*: a 256 KiB
/// datagram cannot exist, and an over-cap send fails with EMSGSIZE, silently
/// losing the whole event.
const MAX_PAYLOAD: usize = 60 * 1024; // 61440

/// The same limit as a `u32`, so the header length needs no `as` cast.
const MAX_PAYLOAD_U32: u32 = 60 * 1024;

// Compile-time, not a runtime assertion: the whole point is that an over-cap
// datagram can never be constructed, and a test would only prove it after the
// binary already shipped.
const _: () = assert!(HEADER_LEN + MAX_PAYLOAD <= 65_507);

/// Set in the tag's high bit when stdin exceeded `MAX_PAYLOAD`. The daemon treats
/// a truncated event as notification-only and backfills detail from the JSONL
/// transcript (PRD §4.4).
const TAG_TRUNCATED: u32 = 0x8000_0000;

/// Hard ceiling on total process lifetime. A blocking hook stalls a real tool
/// call; Claude Code's own timeout is 5 s, which is 5 s of stall. This caps the
/// worst case at half a second even if stdin never reaches EOF.
const WATCHDOG_MS: u64 = 500;

/// Last-resort endpoint when neither the env var nor the endpoint file is
/// usable. Unregistered with IANA; picked once and frozen. Deliberately below
/// the OS ephemeral range (measured 49152+ here) so it can never be taken by an
/// anonymous client socket.
const DEFAULT_PORT: u16 = 45177;

/// Explicit override, injected by the daemon into agents it launches itself.
const ENDPOINT_ENV: &str = "POLIS_HOOK_ENDPOINT";

fn main() {
    // Belt and braces on top of `panic = "abort"`: the panic hook runs *before*
    // the abort, so this converts any panic anywhere into a clean exit(0).
    // Without it a panic exits 0xC0000409 and cancels the agent's tool call.
    std::panic::set_hook(Box::new(|_| std::process::exit(0)));

    // Nothing below may outlive this.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(WATCHDOG_MS));
        std::process::exit(0);
    });

    deliver();

    // The only intended exit in the program.
    std::process::exit(0);
}

/// Everything fallible lives here. Every error is swallowed on purpose.
fn deliver() {
    // ---- 1. event-kind tag from argv --------------------------------------
    // Passed by the settings.json hooks block as `--event <Name>` (exec form,
    // no shell). The hook never parses JSON to learn which event it is.
    // `args_os` and not `args`: `args()` panics on non-UTF-8 arguments.
    let mut tag: u32 = 0;
    let mut want = false;
    for a in std::env::args_os().skip(1) {
        if want {
            tag = a.to_str().map_or(0, event_tag);
            break;
        }
        want = a == *"--event";
    }
    tag &= !TAG_TRUNCATED;

    // ---- 2. read stdin, capped --------------------------------------------
    let mut buf: Vec<u8> = Vec::with_capacity(HEADER_LEN + 16 * 1024);
    buf.extend_from_slice(&[0u8; HEADER_LEN]);

    let stdin = std::io::stdin();
    let mut locked = stdin.lock();
    {
        // One byte past the cap, so overflow is detectable without a second
        // syscall. A read error mid-stream is fine: ship whatever arrived.
        let mut capped = (&mut locked).take((MAX_PAYLOAD + 1) as u64);
        let _ = capped.read_to_end(&mut buf);
    }

    let truncated = buf.len() > HEADER_LEN + MAX_PAYLOAD;
    if truncated {
        buf.truncate(HEADER_LEN + MAX_PAYLOAD);
        tag |= TAG_TRUNCATED;
    }

    // `buf` was truncated to HEADER_LEN + MAX_PAYLOAD above, so the fallback is
    // unreachable; `try_from` simply avoids an `as` cast on a length.
    let payload_len = u32::try_from(buf.len() - HEADER_LEN).unwrap_or(MAX_PAYLOAD_U32);
    buf[0..4].copy_from_slice(&tag.to_le_bytes());
    buf[4..8].copy_from_slice(&payload_len.to_le_bytes());

    // ---- 3. resolve endpoint ----------------------------------------------
    let endpoint = resolve_endpoint();

    // ---- 4. exactly one non-blocking datagram send ------------------------
    // bind(127.0.0.1:0) takes an ephemeral source port: a fixed source port
    // would collide across concurrent hook processes.
    //
    // Binding the SOURCE to 127.0.0.1 is a security control, not an accident:
    // measured, a 127.0.0.1-bound socket cannot reach a non-loopback address at
    // all. Hook payloads contain source code, so a tampered endpoint file must
    // not be able to ship them off the box.
    //
    // Unconnected `send_to`, never `connect()` + `send()`: a *connected* UDP
    // socket on Windows latches ICMP port-unreachable feedback and fails a
    // later call with WSAECONNRESET. An unconnected socket has no connection
    // state to attach that error to.
    if let Ok(sock) = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)) {
        // Redundant for a single send into an empty socket buffer, but it is
        // the contract: under no circumstance may this call park.
        let _ = sock.set_nonblocking(true);
        // Result deliberately discarded. WouldBlock, ConnectionReset,
        // MessageSize, anything at all: drop the event and go.
        let _ = sock.send_to(&buf, endpoint);
    }

    // ---- 5. drain the rest of stdin, AFTER the send -----------------------
    // Exiting with bytes still in the pipe makes the agent's write fail with
    // EPIPE / ERROR_NO_DATA, which is the class of disruption this binary
    // exists to avoid. Draining after the send means an oversized payload
    // delays nothing that matters, and the watchdog bounds it.
    if truncated {
        let _ = std::io::copy(&mut locked, &mut std::io::sink());
    }
}

/// env var -> endpoint file -> compiled-in default. Cheapest first.
fn resolve_endpoint() -> SocketAddrV4 {
    if let Some(v) = std::env::var_os(ENDPOINT_ENV) {
        if let Some(a) = v.to_str().and_then(parse_endpoint) {
            return a;
        }
    }
    if let Some(p) = endpoint_file_path() {
        // Bounded read: a corrupt or hostile endpoint file must not be able to
        // make us read a gigabyte. 128 bytes is far more than
        // `255.255.255.255:65535\n` could ever need.
        if let Ok(f) = std::fs::File::open(&p) {
            let mut head = [0u8; 128];
            let mut n = 0usize;
            let mut r = f.take(128);
            while n < head.len() {
                match r.read(&mut head[n..]) {
                    Ok(0) | Err(_) => break,
                    Ok(k) => n += k,
                }
            }
            // First line only; the daemon may append diagnostics after it.
            // Lossy on purpose: a corrupt file must not be able to bail us out
            // of the lookup, it must just fail to parse.
            let s = String::from_utf8_lossy(&head[..n]);
            if let Some(a) = s.lines().next().and_then(parse_endpoint) {
                return a;
            }
        }
    }
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, DEFAULT_PORT)
}

/// Strictly a literal IPv4 loopback `a.b.c.d:port`.
///
/// Deliberately NOT `ToSocketAddrs`: a hostname would put a DNS lookup on the
/// agent's critical path — measured at milliseconds for a failing name, most of
/// the entire in-process budget — for a value that is always 127.0.0.0/8.
/// Rejecting non-loopback is also what confines a tampered endpoint file.
fn parse_endpoint(s: &str) -> Option<SocketAddrV4> {
    let a: SocketAddrV4 = s.trim().parse().ok()?;
    if a.port() != 0 && a.ip().is_loopback() {
        Some(a)
    } else {
        None
    }
}

/// `%LOCALAPPDATA%\polis\endpoint` — per-user, ACLed to the user by Windows,
/// and present on every Windows install. Not `%TEMP%`: world-writable in
/// multi-user configurations.
#[cfg(windows)]
fn endpoint_file_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let mut p = std::path::PathBuf::from(base);
    p.push("polis");
    p.push("endpoint");
    Some(p)
}

/// `$XDG_RUNTIME_DIR/polis/endpoint`, falling back to `$XDG_STATE_HOME` and
/// then `~/.local/state` — XDG_RUNTIME_DIR is unset in plenty of environments
/// (it is unset in this one), and a hook that only looks there finds nothing.
#[cfg(not(windows))]
fn endpoint_file_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("XDG_STATE_HOME").map(std::path::PathBuf::from))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })?;
    Some(base.join("polis").join("endpoint"))
}

/// The event-kind table. Kept identical to the `--event` names written by
/// `polis install-hooks`; the daemon holds the mirror image of this match.
/// An unknown name is tag 0 (Unknown) and is still delivered — the daemon
/// re-derives the true kind from `hook_event_name` in the payload, which is
/// authoritative. The argv tag is a routing hint, not a source of truth.
fn event_tag(name: &str) -> u32 {
    match name {
        "SessionStart" => 1,
        "SessionEnd" => 2,
        "PreToolUse" => 3,
        "PostToolUseFailure" => 4,
        "PermissionRequest" => 5,
        "SubagentStart" => 6,
        "SubagentStop" => 7,
        "TaskCreated" => 8,
        "TaskCompleted" => 9,
        "Stop" => 10,
        "StopFailure" => 11,
        "TeammateIdle" => 12,
        "Notification" => 13,
        "Elicitation" => 14,
        "ElicitationResult" => 15,
        "WorktreeRemove" => 16,
        "CwdChanged" => 17,
        "PreCompact" => 18,
        "PostCompact" => 19,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_bit_cannot_be_forged_by_a_kind() {
        for name in ["SessionStart", "PostCompact", "nonsense", ""] {
            assert_eq!(event_tag(name) & TAG_TRUNCATED, 0);
        }
    }

    #[test]
    fn event_tags_are_unique_and_in_range() {
        let names = [
            "SessionStart",
            "SessionEnd",
            "PreToolUse",
            "PostToolUseFailure",
            "PermissionRequest",
            "SubagentStart",
            "SubagentStop",
            "TaskCreated",
            "TaskCompleted",
            "Stop",
            "StopFailure",
            "TeammateIdle",
            "Notification",
            "Elicitation",
            "ElicitationResult",
            "WorktreeRemove",
            "CwdChanged",
            "PreCompact",
            "PostCompact",
        ];
        let mut seen: Vec<u32> = names.iter().map(|n| event_tag(n)).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), names.len(), "duplicate event tag");
        assert_eq!(event_tag("WorktreeCreate"), 0, "must never be registered");
    }

    #[test]
    fn rejects_everything_that_is_not_literal_loopback_v4() {
        for bad in [
            "",
            "  ",
            "garbage",
            "1.2.3.4",
            "127.0.0.1",
            "127.0.0.1:0",
            "127.0.0.1:99999",
            "127.0.0.1:-5",
            "127.0.0.1:notaport",
            "localhost:45177",           // would cost a DNS lookup
            "nonexistent.invalid:45177", // would cost a FAILING DNS lookup
            "192.168.1.5:45177",         // off-box
            "0.0.0.0:45177",
            "[::1]:45177", // v6 loopback: not supported, by choice
            "/run/user/1000/polis.sock",
            "127.0.0.1:45177:45178",
        ] {
            assert!(parse_endpoint(bad).is_none(), "should reject {bad:?}");
        }
        assert_eq!(parse_endpoint("127.0.0.1:45177").unwrap().port(), 45177);
        assert!(parse_endpoint(" 127.0.0.9:1 \n").is_some());
    }
}
