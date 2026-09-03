//! Where the session daemon listens, and how a client proves it may connect.
//!
//! # Loopback TCP, not a named pipe
//!
//! `docs/roadmap/terminal-integration.md` planned a named pipe on Windows and a
//! Unix socket elsewhere, and called that "the only genuinely platform-forked
//! code in M8". It is a loopback `TcpListener` instead, and there is no forked
//! code at all:
//!
//! * `std` has no named-pipe API. `CreateNamedPipeW` means either raw FFI —
//!   which the workspace's `unsafe_code = "deny"` rules out, and which
//!   `polis_ingest::hook_listener` already refused for the same reason — or a
//!   Windows-only crate whose Unix twin is a second code path.
//! * Polis already binds fixed loopback ports: OTLP on 4317 and the hook
//!   receiver on 45177 (ADR-0026). This is the third, not the first.
//! * PRD §2's "no server" clause means no cloud, no auth and nothing leaving the
//!   box. `127.0.0.1` satisfies all three: the listener is unreachable from the
//!   network, and [`bind`] refuses to bind anything else.
//!
//! # What the token is for
//!
//! A named pipe carries an ACL and a Unix socket carries file permissions; a
//! loopback port carries neither. Any process on the machine can connect —
//! including a web page, which can `fetch` a `localhost` port and, without a
//! check, would be talking to something that spawns processes on request.
//!
//! So [`Endpoint::token`] is 256 bits written to a file only this user can read,
//! and [`crate::proto::Request::Hello`] must present it before any other call is
//! served. A page in a browser cannot read the file, so it cannot get past
//! `hello` — and it cannot even send one, because the daemon speaks a protocol
//! no cross-origin `fetch` can produce.
//!
//! This is the same shape Jupyter uses for the same reason, and it is honest
//! about what it defends: a local process running **as this user** can read the
//! token file, and could equally read `~/.claude` directly. The token is a fence
//! against other users and other origins, not against the operator.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The port the daemon prefers.
///
/// One above `polis_events::DEFAULT_HOOK_PORT` (45177), so the two live
/// together in a range an operator can allow through a firewall in one rule.
pub const DEFAULT_PORT: u16 = 45178;

/// The file under the state directory that says where the daemon is.
///
/// Next to the hook endpoint file and the corpus, in
/// `polis_app::config::Config::default_state_dir`.
pub const ENDPOINT_FILE: &str = "session.endpoint";

/// Where the daemon is, and the secret needed to talk to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    /// Always on `127.0.0.1`. [`bind`] will not produce anything else.
    pub addr: SocketAddr,
    /// 256 bits of hex. Presented in `hello`, compared in constant time.
    pub token: String,
    /// The daemon's process id, so `polis doctor` can name the process to kill.
    pub pid: u32,
    /// The daemon's [`crate::proto::PROTOCOL`], so a client can refuse to
    /// connect before opening a socket at all.
    pub protocol: u32,
}

impl Endpoint {
    /// The path [`publish`] writes and [`discover`] reads.
    #[must_use]
    pub fn path_in(state_dir: &Path) -> PathBuf {
        state_dir.join(ENDPOINT_FILE)
    }
}

/// Binds the daemon's listener on loopback, falling back off a busy port.
///
/// The fallback is deliberate and the file is why it is safe: unlike the hook
/// receiver, whose port is baked into every agent's environment and therefore
/// cannot move (ADR-0026), nothing has this port memorised — clients read
/// [`ENDPOINT_FILE`]. So a second daemon does not fail to start, it starts
/// somewhere else, and `AddrInUse` never reaches an operator.
///
/// The scan is `preferred`, then `preferred+1 ..= preferred+15`, then `0` — an
/// ephemeral port the OS chooses.
pub fn bind(preferred: u16) -> io::Result<TcpListener> {
    let mut last = None;
    for port in
        std::iter::once(preferred).chain(preferred.saturating_add(1)..=preferred.saturating_add(15))
    {
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
        match TcpListener::bind(addr) {
            Ok(listener) => return Ok(listener),
            Err(error) => last = Some(error),
        }
    }
    TcpListener::bind(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)))
        .map_err(|error| last.unwrap_or(error))
}

/// Writes the endpoint file, readable by this user and nobody else.
///
/// The write is atomic — a temporary file beside the target, then a rename — so
/// a client can never read a half-written token and conclude the daemon is
/// unreachable.
pub fn publish(state_dir: &Path, endpoint: &Endpoint) -> io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let target = Endpoint::path_in(state_dir);
    let temporary = target.with_extension("endpoint.new");
    let body = serde_json::to_string_pretty(endpoint).map_err(io::Error::other)?;
    std::fs::write(&temporary, body)?;
    restrict_to_owner(&temporary)?;
    std::fs::rename(&temporary, &target)
}

/// The state directory the daemon and the window both look in.
///
/// `%LOCALAPPDATA%\polis` on Windows; `$XDG_STATE_HOME/polis` with a
/// `~/.local/state` fallback elsewhere — `XDG_RUNTIME_DIR` is measurably unset
/// in real environments, so it cannot be the only lookup.
///
/// This is the same rule as `polis_app::config::Config::default_state_dir`, and
/// it lives **here** because `polis-sessiond` needs it and must not depend on
/// the window; that module delegates to this one so the two cannot drift and
/// leave a daemon publishing where no client looks.
#[must_use]
pub fn default_state_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("polis"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
            .map(|base| base.join("polis"))
    }
}

/// Reads the endpoint file, if a daemon has published one.
///
/// A missing, unreadable or malformed file all mean the same thing to a caller —
/// there is no daemon here — so they are one `None` rather than three errors.
#[must_use]
pub fn discover(state_dir: &Path) -> Option<Endpoint> {
    let body = std::fs::read_to_string(Endpoint::path_in(state_dir)).ok()?;
    serde_json::from_str(&body).ok()
}

/// Removes the endpoint file. Called on the daemon's way out; silent on failure.
///
/// A stale file is not fatal — a client that finds one and cannot connect says
/// "no daemon is running" — but leaving one behind makes `polis doctor` lie.
pub fn withdraw(state_dir: &Path) {
    let _ = std::fs::remove_file(Endpoint::path_in(state_dir));
}

/// 256 bits of hex for [`Endpoint::token`].
///
/// `RandomState`'s seed comes from the operating system's own randomness —
/// `BCryptGenRandom` on Windows, `getrandom` on Linux — which is what makes this
/// a real secret and not a timestamp in a hat. Four independent `RandomState`s
/// are each asked to hash a distinct value, so the 64 bits observable through
/// `finish()` differ in what they mix as well as in their key.
///
/// No `rand` crate: this is one value, once, at daemon start-up, and the
/// workspace has been asked twice already to write out what it needs instead of
/// taking a dependency for it (ADR-0050).
#[must_use]
pub fn fresh_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::fmt::Write as _;
    use std::hash::{BuildHasher as _, Hash as _, Hasher as _};

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut token = String::with_capacity(64);
    for round in 0u64..4 {
        let mut hasher = RandomState::new().build_hasher();
        round.hash(&mut hasher);
        nanos.hash(&mut hasher);
        std::process::id().hash(&mut hasher);
        std::ptr::addr_of!(token).hash(&mut hasher);
        let _ = write!(token, "{:016x}", hasher.finish());
    }
    token
}

/// Compares two tokens without leaking where they first differ.
///
/// Over loopback the timing signal is small, and it is six lines to remove it.
#[must_use]
pub fn token_matches(expected: &str, offered: &str) -> bool {
    if expected.len() != offered.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in expected.bytes().zip(offered.bytes()) {
        difference |= a ^ b;
    }
    difference == 0
}

/// Sets owner-only permissions on a file. A no-op where the concept is not
/// expressed as a mode — which is why the `Result` looks pointless on Windows
/// and is not on the platform that can actually fail.
#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
fn restrict_to_owner(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        // `%LOCALAPPDATA%` is already per-user, and Windows has no one-call
        // equivalent that does not mean an ACL API and `unsafe`.
        let _ = path;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Framing over a stream
// ---------------------------------------------------------------------------

/// Reads newline-delimited JSON off a socket, one value at a time.
///
/// Separate from [`Wire`] so a caller can put the reader on one thread and the
/// writer on another without a lock between them — which is exactly what both
/// the daemon's connection handler and the window's client do.
#[derive(Debug)]
pub struct Frames<R> {
    reader: BufReader<R>,
    line: String,
}

impl<R: io::Read> Frames<R> {
    /// Wraps a stream.
    pub fn new(inner: R) -> Self {
        Self {
            reader: BufReader::with_capacity(64 * 1024, inner),
            line: String::new(),
        }
    }

    /// The next value, or `None` at end of stream.
    ///
    /// Named `recv` rather than `next` so it cannot be mistaken for an
    /// `Iterator`: it is fallible in a way an iterator is not, and a caller that
    /// treats a parse error as end of stream desynchronises the wire silently.
    ///
    /// A line that does not parse is an error the caller decides about; it is
    /// never silently skipped, because a desynchronised wire that keeps going is
    /// worse than one that stops and says so.
    pub fn recv<T: serde::de::DeserializeOwned>(&mut self) -> io::Result<Option<T>> {
        self.line.clear();
        if self.reader.read_line(&mut self.line)? == 0 {
            return Ok(None);
        }
        let trimmed = self.line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            return Ok(Some(
                serde_json::from_str("null").map_err(io::Error::other)?,
            ));
        }
        serde_json::from_str(trimmed)
            .map(Some)
            .map_err(io::Error::other)
    }
}

/// Writes newline-delimited JSON to a socket.
#[derive(Debug)]
pub struct Wire<W> {
    writer: W,
}

impl<W: Write> Wire<W> {
    /// Wraps a stream.
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Sends one value as one line, flushed.
    ///
    /// Flushed every time on purpose: the thing on the other end is a person
    /// waiting for a keystroke to reach an agent, and a buffered `\x03` is an
    /// interrupt that did not happen.
    pub fn send<T: serde::Serialize>(&mut self, value: &T) -> io::Result<()> {
        let line = crate::proto::line(value).map_err(io::Error::other)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()
    }
}

/// Connects to a published endpoint, with `TCP_NODELAY` set.
///
/// Nagle's algorithm would hold a single-byte keystroke for up to 40 ms waiting
/// for company. On a socket whose entire purpose is one keystroke at a time,
/// that is the difference between a terminal and a bad terminal.
pub fn connect(endpoint: &Endpoint) -> io::Result<TcpStream> {
    let stream = TcpStream::connect(endpoint.addr)?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon publishes here and the window looks here. If they disagree,
    /// every `polis work` reports a daemon that is running fine.
    #[test]
    fn the_state_directory_is_under_the_platform_home() {
        let dir = default_state_dir().expect("every supported platform has one");
        assert!(dir.ends_with("polis"), "{}", dir.display());
        assert!(dir.is_absolute(), "{}", dir.display());
    }

    #[test]
    fn a_published_endpoint_reads_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(discover(dir.path()).is_none(), "nothing published yet");
        let endpoint = Endpoint {
            addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 45178)),
            token: fresh_token(),
            pid: std::process::id(),
            protocol: crate::proto::PROTOCOL,
        };
        publish(dir.path(), &endpoint).expect("publish");
        let back = discover(dir.path()).expect("discover");
        assert_eq!(back.addr, endpoint.addr);
        assert_eq!(back.token, endpoint.token);
        withdraw(dir.path());
        assert!(discover(dir.path()).is_none(), "withdrawn");
    }

    /// The token is the only thing standing between a loopback port that spawns
    /// processes and every other program on the machine.
    #[test]
    fn tokens_are_long_hex_and_do_not_repeat() {
        let tokens: std::collections::BTreeSet<String> = (0..32).map(|_| fresh_token()).collect();
        assert_eq!(tokens.len(), 32, "32 draws, 32 distinct tokens");
        for token in &tokens {
            assert_eq!(token.len(), 64, "256 bits of hex: {token}");
            assert!(token.chars().all(|c| c.is_ascii_hexdigit()), "{token}");
            assert!(
                token.chars().any(|c| c != '0'),
                "an all-zero token means the entropy source failed: {token}"
            );
        }
    }

    #[test]
    fn token_comparison_rejects_the_near_miss() {
        let token = fresh_token();
        assert!(token_matches(&token, &token));
        assert!(!token_matches(&token, &token[..63]));
        let mut nearly = token.clone();
        nearly.pop();
        nearly.push(if token.ends_with('0') { '1' } else { '0' });
        assert!(!token_matches(&token, &nearly));
    }

    /// The listener must never be reachable from the network, whatever else
    /// changes about it.
    #[test]
    fn the_listener_is_loopback_only() {
        let listener = bind(0).expect("bind");
        let addr = listener.local_addr().expect("addr");
        assert!(addr.ip().is_loopback(), "{addr}");
    }

    /// A busy preferred port must not be an error the operator ever sees: no
    /// agent has this port memorised, so the daemon moves and says where it went.
    #[test]
    fn a_busy_port_moves_the_listener_rather_than_failing() {
        let first = bind(DEFAULT_PORT).expect("first bind");
        let second = bind(DEFAULT_PORT).expect("second bind must succeed elsewhere");
        assert_ne!(
            first.local_addr().expect("addr").port(),
            second.local_addr().expect("addr").port()
        );
    }

    #[test]
    fn frames_round_trip_over_a_pipe() {
        use crate::proto::{Call, PaneId, Request};

        let mut buffer: Vec<u8> = Vec::new();
        {
            let mut wire = Wire::new(&mut buffer);
            wire.send(&Call {
                id: 1,
                request: Request::List,
            })
            .expect("send");
            wire.send(&Call {
                id: 2,
                request: Request::Write {
                    pane: PaneId(7),
                    data: b"\x1b[200~hello\x1b[201~".to_vec(),
                },
            })
            .expect("send");
        }
        let mut frames = Frames::new(io::Cursor::new(buffer));
        let first: Call = frames.recv().expect("read").expect("some");
        let second: Call = frames.recv().expect("read").expect("some");
        assert_eq!(first.id, 1);
        assert!(matches!(first.request, Request::List));
        match second.request {
            Request::Write { pane, data } => {
                assert_eq!(pane, PaneId(7));
                assert_eq!(data, b"\x1b[200~hello\x1b[201~");
            }
            other => panic!("wrong arm: {other:?}"),
        }
        assert!(frames.recv::<Call>().expect("read").is_none(), "eof");
    }
}
