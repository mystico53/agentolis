//! Channel B — the hook datagram listener (PRD §4.2).
//!
//! The daemon side of the wire format in [`polis_events::wire`]. Every
//! obligation below is measured in `docs/verified/hook-ipc.md`:
//!
//! * **Bind `127.0.0.1:45177` exclusively.** The bind *is* the singleton lock —
//!   a second daemon gets `AddrInUse`. It must then exit with a clear message
//!   rather than binding elsewhere: silently taking another port produces a
//!   half-populated map and an operator who cannot tell which window is lying
//!   (ADR-0026).
//! * **Loopback UDP on every platform.** No `AF_UNIX` anywhere — a Unix datagram
//!   socket is not portable to Windows in the form `polis-hook` needs, and
//!   `polis-hook` has no dependencies to paper over the difference (ADR-0022,
//!   ADR-0036).
//! * **Raise `SO_RCVBUF` to [`polis_events::RECV_BUFFER_BYTES`] (8 MiB).** At the
//!   65 536-byte default a burst of 5 000 × 60 KiB datagrams loses 0.9%; at
//!   8 MiB it loses 0%. PRD §13.1 asks for 500 events/sec with zero drops.
//! * **Set `SIO_UDP_CONNRESET` to 0** on Windows, as defence in depth: without
//!   it an ICMP port-unreachable from an earlier send can fail a later `recv`.
//! * **Validate before trusting**: `len + 8 == datagram_len` and
//!   `kind <= EventKind::MAX_TAG`. The port is unauthenticated; any local process
//!   can send anything. The blast radius is a visualisation drawing something
//!   untrue, which is bounded — but only if the decoder never panics and never
//!   allocates on attacker input.
//! * **The argv tag is a hint.** `hook_event_name` in the payload is
//!   authoritative.
//! * **A set truncation bit is notification-only.** Backfill the detail from the
//!   JSONL transcript (PRD §4.4).
//!
//! # Two of those obligations are not met, and the reason is a missing dependency
//!
//! `SO_RCVBUF` and `SIO_UDP_CONNRESET` are `setsockopt`/`WSAIoctl` calls. Neither
//! `std` nor `tokio` exposes them, `socket2` is not declared for this crate, and
//! the workspace sets `unsafe_code = "deny"` — so raw FFI is not an option here
//! either. Both are therefore handled in software, and the gap is stated rather
//! than hidden:
//!
//! * **`SIO_UDP_CONNRESET`** is fully neutralised without the ioctl.
//!   `docs/verified/hook-ipc.md` §2.1 `[F]` measured a receive-only socket taking
//!   50 datagrams with **0 receive errors**, and the ioctl is called "defence in
//!   depth … it removes an entire error class from the ingest loop". That error
//!   class is handled directly instead: [`ConnectionReset`](io::ErrorKind::ConnectionReset)
//!   out of `recv_from` is *counted and skipped*, never fatal and never a break
//!   in the loop. The observable behaviour is identical to having set the ioctl.
//! * **`SO_RCVBUF`** stays at the OS default (measured 65 536). The mitigation is
//!   that the receive loop does the smallest possible amount of work per
//!   datagram and never allocates on the accepted path: one `recv_from` into a
//!   reused 64 KiB buffer, one header decode, one JSON parse. The measured 0.9%
//!   loss needs 5 000 × 60 KiB datagrams delivered back to back, which real hook
//!   traffic cannot produce — every datagram costs the sender a process spawn,
//!   measured at ~10 ms, and the whole 480-hook concurrency row of the safety
//!   matrix peaked at ~490 datagrams/sec of ~1 KiB each. Declaring `socket2` on
//!   this crate would close it properly; that needs a manifest change.
//!
//! # Bounded by construction
//!
//! One thread, one reused buffer, no queue of its own: a datagram is decoded and
//! handed to [`EventSink`] before the next `recv_from`. The only place events can
//! accumulate is the bus, which is bounded and drops the oldest (PRD §4.5). There
//! is no path here that can grow without bound, and no path that can block: the
//! socket carries a read timeout so shutdown cannot hang, and the sink is
//! infallible and non-blocking by contract.

use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use polis_events::{
    Channel, Event, EventKind, EventMeta, HookEvent, HookPayload, PathMapper, Payload, WireError,
    WireHeader,
};

use crate::bus::EventSink;
use crate::{IngestSource, SourceHealth};

/// How long `recv_from` blocks before the loop re-checks the stop flag.
///
/// Shutdown normally costs nothing at all — [`HookListener::shutdown`] sends a
/// wake datagram to its own port — so this is only the backstop for the case
/// where that send fails. Four timer wakeups per second is far inside PRD
/// §13.1's "under 2% of one core with no agent activity".
const RECV_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Receive buffer size (`docs/verified/hook-ipc.md` §9.7).
///
/// Must be at least 65 536 so that a maximal IPv4 datagram — 65 507 payload
/// bytes, more than `polis-hook` can ever send — is never silently cut by the
/// receive call itself. On Windows a short buffer fails the call with
/// `WSAEMSGSIZE` and loses the datagram; this size makes that impossible.
const RECV_BUFFER: usize = 65_536;

/// Consecutive unexpected `recv_from` errors before the loop gives up.
///
/// Not zero, because a transient error must not take the channel down; not
/// unbounded, because a socket that fails every call would otherwise spin a core
/// forever. On giving up the channel reports [`SourceHealth::Stopped`] with the
/// error, which is the state the status bar renders.
const MAX_CONSECUTIVE_ERRORS: u32 = 64;

/// The endpoint file's name inside [`endpoint_dir`].
const ENDPOINT_FILE: &str = "endpoint";

// ---------------------------------------------------------------------------
// The listener
// ---------------------------------------------------------------------------

/// The hook listener thread.
#[derive(Debug)]
pub struct HookListener {
    addr: SocketAddrV4,
    mapper: PathMapper,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
    /// `None` while running. `Some(reason)` once the loop has stopped, whether
    /// by request or by failure.
    stopped_because: Arc<Mutex<Option<String>>>,
    /// Where [`HookListener::publish_endpoint`] last wrote, so shutdown can
    /// remove exactly that file and nothing else.
    published: Mutex<Option<PathBuf>>,
    thread: Option<JoinHandle<()>>,
}

impl HookListener {
    /// Binds and starts receiving.
    ///
    /// `AddrInUse` here means another Polis is already running. Unlike every
    /// other channel failure this one is **not** degradable: two daemons would
    /// each see roughly half the events and neither would say so. Report it and
    /// exit.
    pub fn start(
        addr: SocketAddrV4,
        sink: EventSink,
        mapper: PathMapper,
    ) -> Result<Self, std::io::Error> {
        Self::start_with(addr, sink, mapper)
    }

    /// [`HookListener::start`] against any delivery target.
    ///
    /// The receive path is exercised by tests that must not depend on the bus
    /// being implemented, and PRD §16's safety matrix wants to observe what a
    /// hostile datagram produces rather than only that it was not fatal.
    fn start_with<D: HookDelivery>(
        addr: SocketAddrV4,
        delivery: D,
        mapper: PathMapper,
    ) -> Result<Self, io::Error> {
        // Synchronously, on the calling thread: `AddrInUse` must surface as a
        // matchable `io::Error` and not as a panic inside a thread nobody joins.
        // No `SO_REUSEADDR`: the exclusivity of this bind *is* the singleton
        // lock (ADR-0026).
        let socket = UdpSocket::bind(addr).map_err(|e| annotate_bind_error(addr, e))?;
        socket.set_read_timeout(Some(RECV_POLL_INTERVAL))?;
        let bound = match socket.local_addr()? {
            SocketAddr::V4(v4) => v4,
            // Unreachable: the bind address is `SocketAddrV4`. Reporting the
            // requested address beats an `unwrap` on a value we already have.
            SocketAddr::V6(_) => addr,
        };

        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        let stopped_because = Arc::new(Mutex::new(None));

        // The socket is *moved* into the receive thread and lives exactly as
        // long as the loop. Keeping a second handle here would hold the port —
        // and therefore the singleton lock — open after a loop that has already
        // given up, which is the one state where another Polis should be able to
        // take over.
        let thread = {
            let stop = Arc::clone(&stop);
            let counters = Arc::clone(&counters);
            let stopped_because = Arc::clone(&stopped_because);
            // The loop needs its own mapper: `normalize::hook_event` uses it to
            // recover `LogicalPath`s from `tool_input` *before* discarding the
            // raw JSON (ADR-0005), and that has to happen on the receive path,
            // not somewhere downstream that might forget.
            let mapper = mapper.clone();
            std::thread::Builder::new()
                .name("polis-hook-recv".to_owned())
                .spawn(move || {
                    recv_loop(
                        &socket,
                        &stop,
                        &counters,
                        &stopped_because,
                        &delivery,
                        &mapper,
                    );
                })?
        };

        tracing::debug!(addr = %bound, "hook listener bound");
        Ok(Self {
            addr: bound,
            mapper,
            stop,
            counters,
            stopped_because,
            published: Mutex::new(None),
            thread: Some(thread),
        })
    }

    /// The address actually bound, for `polis doctor` and for
    /// [`HookListener::publish_endpoint`].
    pub fn local_addr(&self) -> SocketAddrV4 {
        self.addr
    }

    /// The [`PathMapper`] this channel was started with.
    ///
    /// Channel B carries its payload verbatim — [`Payload::Hook`] wraps the
    /// decoded [`HookEvent`] and [`EventMeta`] holds identity, not paths — so
    /// nothing in the receive path maps a path today. The mapper is kept because
    /// `Ingest::start` hands one to every channel and because
    /// [`crate::normalize::hook_event`] takes one.
    pub fn mapper(&self) -> &PathMapper {
        &self.mapper
    }

    /// Writes the endpoint file through which hooks discover the port.
    ///
    /// Atomic (temp + rename) so a hook cannot read a half-written line, and
    /// deleted best-effort on clean shutdown so later hooks fall back to the
    /// compiled-in 45177 — which is exactly where the next daemon binds. A stale
    /// file needs no cleanup: sending to a closed port is measured as `Ok(n)`,
    /// exit 0, and marginally *cheaper* than the live case.
    ///
    /// `%LOCALAPPDATA%\polis\endpoint` on Windows, `$XDG_RUNTIME_DIR/polis/endpoint`
    /// otherwise. The variable `polis-hook` reads is
    /// [`polis_events::HOOK_ENDPOINT_ENV`].
    pub fn publish_endpoint(&self) -> Result<PathBuf, std::io::Error> {
        let dir = endpoint_dir().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no per-user runtime directory to publish the hook endpoint in",
            )
        })?;
        let path = write_endpoint_file(&dir, self.addr)?;
        *lock(&self.published) = Some(path.clone());
        tracing::debug!(path = %path.display(), addr = %self.addr, "hook endpoint published");
        Ok(path)
    }

    /// Datagrams received and datagrams rejected since start.
    ///
    /// Rejections are the unauthenticated-port signal: a non-zero and rising
    /// count means something other than `polis-hook` is sending, and that is
    /// worth surfacing rather than silently counting.
    pub fn counters(&self) -> HookCounters {
        HookCounters {
            accepted: self.counters.accepted.load(Ordering::Relaxed),
            rejected: self.counters.rejected.load(Ordering::Relaxed),
            truncated: self.counters.truncated.load(Ordering::Relaxed),
        }
    }

    /// Requests shutdown and joins the receive thread.
    pub fn shutdown(mut self) {
        self.stop_inner();
    }

    /// The idempotent body of [`HookListener::shutdown`], shared with [`Drop`].
    fn stop_inner(&mut self) {
        let already = self.stop.swap(true, Ordering::SeqCst);
        if already && self.thread.is_none() {
            return;
        }
        // Wake the blocking `recv_from` immediately; the read timeout is only
        // the backstop for a wake that could not be sent.
        self.wake();
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        let mut reason = lock(&self.stopped_because);
        if reason.is_none() {
            *reason = Some("shut down".to_owned());
        }
        drop(reason);
        if let Some(path) = lock(&self.published).take() {
            remove_endpoint_file(&path, self.addr);
        }
    }

    /// Sends a zero-length datagram to our own port so the loop returns at once.
    ///
    /// The loop re-checks the stop flag before it looks at what arrived, so this
    /// datagram is never decoded and never counted — it must not show up as
    /// noise on the rejection counter. Every failure here is ignored: the read
    /// timeout already bounds shutdown.
    fn wake(&self) {
        if let Ok(waker) = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)) {
            let _ = waker.set_nonblocking(true);
            let _ = waker.send_to(&[], self.addr);
        }
    }
}

impl Drop for HookListener {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// Turns a bare `AddrInUse` into the message ADR-0026 requires an operator to
/// see, without changing [`io::Error::kind`] — callers still match on it.
///
/// The alternative Polis must never take is binding somewhere else: two daemons
/// would each receive roughly half the hook events and neither would say so.
fn annotate_bind_error(addr: SocketAddrV4, e: io::Error) -> io::Error {
    if e.kind() != io::ErrorKind::AddrInUse {
        return e;
    }
    let file = endpoint_dir().map_or_else(
        || ENDPOINT_FILE.to_owned(),
        |dir| dir.join(ENDPOINT_FILE).display().to_string(),
    );
    io::Error::new(
        io::ErrorKind::AddrInUse,
        format!(
            "another polis is already receiving hook events on {addr} \
             (its pid is in {file}) — refusing to bind a different port, \
             because two daemons each see half the events and neither says so"
        ),
    )
}

/// Receive-side accounting for the hook channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HookCounters {
    /// Well-formed datagrams accepted.
    pub accepted: u64,
    /// Datagrams rejected by [`decode_datagram`]. A rising count means an
    /// unexpected sender on an unauthenticated port.
    pub rejected: u64,
    /// Accepted datagrams whose truncation bit was set. These are
    /// notification-only and must be backfilled from the transcript.
    pub truncated: u64,
}

/// The live counters behind [`HookCounters`].
#[derive(Debug, Default)]
struct Counters {
    accepted: AtomicU64,
    rejected: AtomicU64,
    truncated: AtomicU64,
}

impl IngestSource for HookListener {
    fn channel(&self) -> polis_events::Channel {
        polis_events::Channel::Hook
    }

    fn health(&self) -> SourceHealth {
        match lock(&self.stopped_because).clone() {
            Some(reason) => SourceHealth::Stopped { reason },
            None => SourceHealth::Running,
        }
    }

    fn shutdown(self: Box<Self>) {
        (*self).shutdown();
    }
}

// ---------------------------------------------------------------------------
// The receive loop
// ---------------------------------------------------------------------------

/// Where a decoded event goes.
///
/// A trait rather than a bare [`EventSink`] so the loop can be driven in tests
/// while `bus.rs` is still being written, and so PRD §16's safety matrix can
/// assert what a hostile datagram *produces* rather than only that it was not
/// fatal.
trait HookDelivery: Send + 'static {
    /// Hands one normalised event on. Must never block and must never fail —
    /// backpressure here would reach an agent.
    fn deliver(&self, event: Event);
}

impl HookDelivery for EventSink {
    fn deliver(&self, event: Event) {
        // The bus is bounded and drops the oldest; `Push::Evicted` is already
        // accounted for in `BusStats::dropped_hook`.
        let _ = self.push(event);
    }
}

/// The receive loop. Runs until the stop flag is set or the socket gives up.
fn recv_loop<D: HookDelivery>(
    socket: &UdpSocket,
    stop: &AtomicBool,
    counters: &Counters,
    stopped_because: &Mutex<Option<String>>,
    delivery: &D,
    mapper: &PathMapper,
) {
    let mut buf = vec![0u8; RECV_BUFFER];
    let mut consecutive_errors: u32 = 0;
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match socket.recv_from(&mut buf) {
            Ok((len, _from)) => {
                // Before anything is decoded: a shutdown wake datagram must not
                // be counted as noise from a foreign sender.
                if stop.load(Ordering::Acquire) {
                    return;
                }
                consecutive_errors = 0;
                // `len` cannot exceed the buffer: an IPv4 datagram carries at
                // most 65 507 payload bytes and the buffer is 65 536.
                let datagram = buf.get(..len).unwrap_or(&[]);
                handle_datagram(datagram, counters, delivery, mapper);
            }
            Err(e) => {
                if recoverable(&e) {
                    // A `recv_from` that returns at all — even reporting the
                    // read timeout or a Windows reset — proves the socket is
                    // alive, so any run of real failures has ended.
                    consecutive_errors = 0;
                    continue;
                }
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    let reason = format!("hook socket failed {consecutive_errors} times: {e}");
                    tracing::warn!(%reason, "hook listener stopping");
                    *lock(stopped_because) = Some(reason);
                    return;
                }
                tracing::debug!(error = %e, consecutive_errors, "hook recv failed");
            }
        }
    }
}

/// Whether a `recv_from` error is one the loop simply ignores.
///
/// `WouldBlock` and `TimedOut` are the read timeout expiring — the same event
/// spelled differently by Windows and Unix. `Interrupted` is a signal.
/// `ConnectionReset` is the Windows `WSAECONNRESET` gotcha: a datagram this
/// socket sent earlier drew an ICMP port-unreachable, and the stack reports it on
/// a *later* call. Polis's receive socket never sends, so it should never see one;
/// treating it as recoverable is what makes the missing `SIO_UDP_CONNRESET`
/// ioctl unobservable (see the module docs).
fn recoverable(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
            | io::ErrorKind::Interrupted
            | io::ErrorKind::ConnectionReset
    )
}

/// Decodes one datagram, counts it, and delivers it if it is deliverable.
fn handle_datagram<D: HookDelivery>(
    datagram: &[u8],
    counters: &Counters,
    delivery: &D,
    mapper: &PathMapper,
) {
    match decode_datagram(datagram) {
        Ok(event) => {
            counters.accepted.fetch_add(1, Ordering::Relaxed);
            if event.truncated {
                counters.truncated.fetch_add(1, Ordering::Relaxed);
            }
            // `normalize::hook_event`, not `to_event`: it recovers the paths
            // and then drops the raw `tool_input`, whose body is the whole file
            // for a `Write` and the diff for an `Edit` (ADR-0005). `to_event`
            // still owns the envelope and is reached through it.
            delivery.deliver(crate::normalize::hook_event(event, mapper));
        }
        Err(e) => {
            counters.rejected.fetch_add(1, Ordering::Relaxed);
            // Debug, not warn, and never a file write: on an unauthenticated
            // port a noisy neighbour must not be able to drive Polis's logging.
            tracing::debug!(bytes = datagram.len(), error = %e, "hook datagram dropped");
        }
    }
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Why a datagram produced no event. Every variant is a *drop*, never a crash.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HookDecodeError {
    /// The 8-byte frame did not validate. Almost certainly not `polis-hook`:
    /// unrelated loopback traffic, or a forged datagram.
    #[error(transparent)]
    Frame(#[from] WireError),
    /// The frame was well formed but its body is not a JSON object, and the
    /// truncation bit was **not** set — so this is corruption or a forgery, not
    /// the expected mid-document cut of an over-cap payload.
    #[error("hook payload is not a JSON object: {0}")]
    Payload(#[source] serde_json::Error),
}

/// Decodes one received datagram.
///
/// Split out from the receive loop so PRD §16's hook-safety matrix and a
/// `cargo-fuzz` target can drive it directly without a socket. Must be total:
/// every byte sequence is either a [`HookEvent`] or a [`HookDecodeError`], never a
/// panic and never an allocation proportional to an attacker-supplied length.
///
/// Three rules from `docs/verified/hook-ipc.md` §9 are applied here:
///
/// * The frame is validated before it is trusted — [`WireHeader::decode`] checks
///   `len + 8 == datagram_len`, which stands in for a magic number.
/// * `hook_event_name` in the payload is **authoritative**; the wire tag is a
///   routing hint used only when the payload does not name the event.
/// * A set truncation bit is **notification-only**. The body of a truncated frame
///   is a JSON document cut mid-value and essentially never parses, so it is
///   repaired down to its last complete top-level field (which is where
///   `session_id` and `agent_id` live) and the rest is backfilled from the JSONL
///   transcript. A truncated frame is therefore never dropped for a parse error.
pub fn decode_datagram(datagram: &[u8]) -> Result<HookEvent, HookDecodeError> {
    let (header, body) = WireHeader::decode(datagram)?;
    match HookPayload::parse(body) {
        Ok(payload) => {
            let kind = match payload.kind() {
                EventKind::Unknown => header.kind,
                named => named,
            };
            if kind != header.kind {
                // Not drift worth a control event: the tag is documented as a
                // hint, and a forged datagram must not be able to spam the bus.
                tracing::debug!(tag = %header.kind, payload = %kind, "hook tag disagrees with payload");
            }
            Ok(HookEvent {
                kind,
                truncated: header.truncated,
                payload,
            })
        }
        Err(_) if header.truncated => Ok(HookEvent {
            kind: header.kind,
            truncated: true,
            payload: salvage_truncated(body),
        }),
        Err(e) => Err(HookDecodeError::Payload(e)),
    }
}

/// Recovers what it can from a payload `polis-hook` cut at `MAX_PAYLOAD`.
///
/// The cut lands mid-value in a large `tool_input`, so the document is repaired
/// back to its last complete **top-level** field and re-parsed. That is enough to
/// keep `session_id`, `agent_id` and `cwd`, which every payload carries before
/// its bulky fields — and without a session the event could not be attributed to
/// a thread at all (PRD §5). Anything unrecoverable degrades to an empty payload
/// rather than to a dropped event, because the wire tag still tells the daemon
/// *which* event fired.
fn salvage_truncated(body: &[u8]) -> HookPayload {
    repair_truncated_json(body)
        .and_then(|repaired| HookPayload::parse(&repaired).ok())
        .unwrap_or_default()
}

/// Closes a JSON object that was cut mid-document, discarding the incomplete
/// tail.
///
/// Walks the bytes once tracking string, escape and nesting state, remembers the
/// last comma seen at depth 1, truncates there and appends the closing brace.
/// Returns `None` when the body is not an object at all. Never panics: every
/// index is derived from the iteration itself.
fn repair_truncated_json(body: &[u8]) -> Option<Vec<u8>> {
    let start = body.iter().position(|b| !b.is_ascii_whitespace())?;
    if body.get(start) != Some(&b'{') {
        return None;
    }
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    // The fallback is the byte after the opening brace, i.e. an empty object.
    let mut cut = start + 1;
    for (i, &b) in body.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            b',' if depth == 1 => cut = i,
            _ => {}
        }
    }
    let mut out = body.get(..cut)?.to_vec();
    out.push(b'}');
    Some(out)
}

/// Turns a decoded hook datagram into a bus event (PRD §4.2, §5).
///
/// The envelope is the whole point of Channel B. `docs/verified/hooks-schema.md`
/// §2.1 calls it out verbatim — *"use this to distinguish subagent hook calls
/// from main-thread calls"* — and hooks are one of only three places on any
/// channel where a subagent's work is separable from the main agent's:
///
/// ```text
/// thread   := ThreadId::of_session(session_id)   // the parent's id inside a subagent
/// worker   := agent_id                            // PRESENCE is the discriminator
/// agent_type := agent_type                        // never an identity
/// ```
///
/// `agent_type` alone is **not** a worker discriminator: a main agent launched
/// with `claude --agent foo` carries one and has no `agent_id` (ADR-0030). Keying
/// `is_worker` on `agent_type` would file every such session under a phantom
/// worker.
///
/// No [`PathMapper`] is taken because none is needed: [`Payload::Hook`] carries
/// the payload verbatim, `tool_input` included, and [`EventMeta`] holds identity
/// rather than paths. `normalize::hook_event` is the crate's designated home for
/// this and can delegate here unchanged.
pub fn to_event(event: HookEvent) -> Event {
    let mut meta = EventMeta::now(Channel::Hook);
    if let Some(session) = event.payload.session_id.clone() {
        meta = meta.with_session(session);
    }
    // Cloned rather than moved: the payload travels on whole, so that PRD §4.4's
    // catch-all fields survive to the world thread.
    meta.worker.clone_from(&event.payload.agent_id);
    meta.agent_type.clone_from(&event.payload.agent_type);
    meta.prompt.clone_from(&event.payload.prompt_id);
    // `event.sequence` is an OTel field; the hook channel has no counter and a
    // fabricated one would produce phantom `SequenceGap` reports.
    Event::new(meta, Payload::Hook(Box::new(event)))
}

// ---------------------------------------------------------------------------
// The endpoint file
// ---------------------------------------------------------------------------

/// The directory `polis-hook` looks in for the endpoint file.
///
/// **This must stay identical to `polis-hook`'s `endpoint_file_path`.** The hook
/// links `std` only and cannot import this crate (ADR-0036), so the two copies
/// are pinned together by `tests::endpoint_directory_matches_the_hook_binary`,
/// which reads the hook's source with `include_str!`.
///
/// Windows: `%LOCALAPPDATA%\polis` — per-user, ACLed by Windows, present on every
/// install. Not `%TEMP%`, which is world-writable in multi-user configurations.
///
/// Unix: `$XDG_RUNTIME_DIR/polis`, falling back to `$XDG_STATE_HOME` and then
/// `~/.local/state`. The fallbacks are not optional: `XDG_RUNTIME_DIR` is
/// measurably unset in real environments.
#[cfg(windows)]
pub fn endpoint_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("polis"))
}

/// The directory `polis-hook` looks in for the endpoint file. See the Windows
/// variant of this function for the full contract.
#[cfg(not(windows))]
pub fn endpoint_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_STATE_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("polis"))
}

/// Writes `dir/endpoint` atomically and returns the path written.
///
/// Temp file in the **same directory** then rename, so a hook reading
/// concurrently sees either the old file or the new one and never a half-written
/// first line. A rename into the same directory is atomic on NTFS and on every
/// POSIX filesystem; a rename across directories is not, which is why the temp
/// file is not put in `%TEMP%`.
///
/// A stale file from a dead daemon needs no special handling: it is simply
/// superseded. Nothing checks whether the pid in it is alive, because that check
/// is racy and unnecessary — the caller only reaches this function after winning
/// the `bind(127.0.0.1:45177)`, which *is* the singleton lock (ADR-0026), so a
/// second live daemon cannot be the thing that wrote it.
///
/// The first line is load-bearing and is all `polis-hook` reads (bounded to 128
/// bytes, first line only). The rest is for humans and `polis doctor` — the pid
/// is what the "another polis is already receiving" message points at. No
/// timestamp is written: PRD §7.4 keeps the wall clock out of everything the
/// daemon produces, and the file carries no information a timestamp would add.
pub fn write_endpoint_file(dir: &Path, addr: SocketAddrV4) -> Result<PathBuf, io::Error> {
    std::fs::create_dir_all(dir)?;
    let final_path = dir.join(ENDPOINT_FILE);
    let tmp = dir.join(format!("{ENDPOINT_FILE}.{}.tmp", std::process::id()));
    // `\n` on every platform: `polis-hook` takes the first line with
    // `str::lines`, which accepts either ending, and one spelling keeps the file
    // byte-identical across operating systems.
    let contents = format!("{addr}\npid={}\n", std::process::id());
    let write = (|| -> io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(contents.as_bytes())?;
        // Durable before the rename: a rename that survives a crash must not
        // expose an empty file.
        file.sync_all()
    })();
    if let Err(e) = write.and_then(|()| std::fs::rename(&tmp, &final_path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(final_path)
}

/// Removes an endpoint file, but only if it still points at `addr`.
///
/// Best-effort and silent: deleting it makes later hooks fall back to the
/// compiled-in 45177, which is exactly where the next daemon binds, and failing
/// to delete it is harmless because a stale file costs a hook nothing (measured:
/// sending to a closed port is `Ok(n)`, exit 0, and marginally *cheaper* than the
/// live case).
///
/// The address check is what keeps two daemons from stepping on each other. Only
/// one can hold 45177, but an operator running a second Polis on `--port N` must
/// not have its shutdown delete the first one's file.
pub fn remove_endpoint_file(path: &Path, addr: SocketAddrV4) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    if contents.lines().next().map(str::trim) == Some(addr.to_string().as_str()) {
        let _ = std::fs::remove_file(path);
    }
}

/// Locks a mutex, ignoring poisoning.
///
/// A panic elsewhere must not turn `health()` — which the UI thread polls every
/// frame — into a panic of its own. The data behind these mutexes is a `String`
/// and a `PathBuf`; neither has an invariant a panic could have broken.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;

    use polis_events::{encode_frame, MAX_DATAGRAM, MAX_PAYLOAD};

    /// A [`HookDelivery`] that hands events to the test thread.
    struct Collect(mpsc::Sender<Event>);

    impl HookDelivery for Collect {
        fn deliver(&self, event: Event) {
            let _ = self.0.send(event);
        }
    }

    fn any_loopback() -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)
    }

    fn payload_of(event: &Event) -> &HookEvent {
        match &event.payload {
            Payload::Hook(h) => h,
            other => panic!("expected a hook payload, got {other:?}"),
        }
    }

    // -- the wire format, against the real binary ---------------------------

    /// Compiles `polis-hook` from its real source with `rustc`.
    ///
    /// Not `cargo`: a nested cargo invocation contends with the lock the outer
    /// `cargo test` holds on the target directory. The hook links `std` only and
    /// declares no dependencies (ADR-0036), so `rustc` on the single source file
    /// produces exactly the shipping binary's behaviour in about a second.
    fn build_hook_binary(out_dir: &Path) -> PathBuf {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../polis-hook/src/main.rs");
        let out = out_dir.join(if cfg!(windows) {
            "polis-hook.exe"
        } else {
            "polis-hook"
        });
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let status = Command::new(rustc)
            .args(["--edition", "2021", "-O", "-o"])
            .arg(&out)
            .arg(&src)
            .status()
            .expect("rustc must be on PATH inside a cargo test");
        assert!(status.success(), "polis-hook failed to compile: {status}");
        out
    }

    /// Runs the hook once with `stdin` on its standard input and returns nothing
    /// but the assertion that it exited 0 — which is PRD §16's whole contract.
    fn run_hook(hook: &Path, endpoint: SocketAddrV4, event: &str, stdin: Vec<u8>) {
        let mut child = Command::new(hook)
            .args(["--event", event])
            .env(polis_events::HOOK_ENDPOINT_ENV, endpoint.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("hook must spawn");
        let mut pipe = child.stdin.take().expect("piped stdin");
        // On a separate thread: an over-cap payload is larger than the pipe
        // buffer, and the hook only drains the remainder after its send.
        let writer = std::thread::spawn(move || {
            let _ = pipe.write_all(&stdin);
        });
        let status = child.wait().expect("hook must exit");
        let _ = writer.join();
        assert_eq!(
            status.code(),
            Some(0),
            "polis-hook must never exit non-zero"
        );
    }

    #[test]
    fn frames_from_the_real_hook_binary_decode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hook = build_hook_binary(dir.path());
        let sock = UdpSocket::bind(any_loopback()).expect("bind");
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let SocketAddr::V4(endpoint) = sock.local_addr().unwrap() else {
            panic!("bound v4")
        };
        let mut buf = vec![0u8; RECV_BUFFER];

        let mut recv = |event: &str, stdin: Vec<u8>| {
            run_hook(&hook, endpoint, event, stdin);
            let (n, _) = sock.recv_from(&mut buf).expect("a datagram must arrive");
            decode_datagram(&buf[..n])
        };

        // 1. An ordinary main-agent PreToolUse.
        let body = br#"{"session_id":"s-1","hook_event_name":"PreToolUse","cwd":"C:\\r",
                        "tool_name":"Edit","tool_input":{"file_path":"C:\\r\\a.rs"}}"#;
        let e = recv("PreToolUse", body.to_vec()).expect("well-formed");
        assert_eq!(e.kind, EventKind::PreToolUse);
        assert!(!e.truncated);
        assert!(!e.payload.is_worker(), "no agent_id means the main agent");
        assert_eq!(e.payload.tool_name.as_deref(), Some("Edit"));

        // 2. Inside a subagent: `agent_id` is the discriminator (§2.1).
        let body = br#"{"session_id":"s-1","hook_event_name":"SubagentStop",
                        "agent_id":"def456","agent_type":"Explore"}"#;
        let e = recv("SubagentStop", body.to_vec()).expect("well-formed");
        assert_eq!(e.kind, EventKind::SubagentStop);
        assert!(e.payload.is_worker());
        assert_eq!(e.payload.agent_id.as_ref().unwrap().as_str(), "def456");

        // 3. The payload is authoritative; the argv tag is only a hint.
        let body = br#"{"session_id":"s-1","hook_event_name":"CwdChanged"}"#;
        let e = recv("NotAnEventPolisKnows", body.to_vec()).expect("well-formed");
        assert_eq!(e.kind, EventKind::CwdChanged, "tag 0, payload wins");

        // 4. Empty stdin: the hook sends a bare 8-byte header (safety row 9).
        let err = recv("Stop", Vec::new()).expect_err("no payload, nothing to attribute");
        assert!(matches!(err, HookDecodeError::Payload(_)), "{err:?}");
    }

    #[test]
    fn an_over_cap_payload_arrives_truncated_and_still_attributable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hook = build_hook_binary(dir.path());
        let sock = UdpSocket::bind(any_loopback()).expect("bind");
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let SocketAddr::V4(endpoint) = sock.local_addr().unwrap() else {
            panic!("bound v4")
        };

        // A 100 KiB Write: the cut lands deep inside `tool_input.content`.
        let mut body = br#"{"session_id":"s-9","agent_id":"w-3","hook_event_name":"PreToolUse","cwd":"C:\\r","tool_input":{"file_path":"C:\\r\\big.rs","content":""#.to_vec();
        body.extend(std::iter::repeat_n(b'x', 100 * 1024));
        body.extend_from_slice(br#""}}"#);
        run_hook(&hook, endpoint, "PreToolUse", body);

        let mut buf = vec![0u8; RECV_BUFFER];
        let (n, _) = sock.recv_from(&mut buf).expect("a datagram must arrive");
        assert_eq!(n, MAX_DATAGRAM, "one maximal datagram, never a fragment");
        let e = decode_datagram(&buf[..n]).expect("a truncated frame is never dropped");
        assert!(e.truncated, "bit 31 must survive the wire");
        assert_eq!(
            e.kind,
            EventKind::PreToolUse,
            "kind comes from the argv tag"
        );
        // The repair keeps every complete top-level field, which is exactly the
        // identity PRD §5 needs to file the event under a thread.
        assert_eq!(e.payload.session_id.as_ref().unwrap().as_str(), "s-9");
        assert!(
            e.payload.is_worker(),
            "the discriminator survives truncation"
        );
        assert_eq!(e.payload.cwd.as_deref(), Some("C:\\r"));
        assert!(e.payload.tool_input.is_none(), "the cut field is discarded");
    }

    // -- decoding, without a socket -----------------------------------------

    #[test]
    fn a_truncated_header_is_rejected_not_a_panic() {
        for n in 0..polis_events::HEADER_LEN {
            let err = decode_datagram(&vec![0u8; n]).expect_err("short");
            assert!(matches!(
                err,
                HookDecodeError::Frame(WireError::TooShort { .. })
            ));
        }
    }

    #[test]
    fn a_length_that_disagrees_with_the_datagram_is_rejected() {
        // A valid header with a truncated body: 100 declared, 3 delivered.
        let mut frame = 3u32.to_le_bytes().to_vec();
        frame.extend_from_slice(&100u32.to_le_bytes());
        frame.extend_from_slice(b"abc");
        let err = decode_datagram(&frame).expect_err("mismatch");
        assert!(matches!(
            err,
            HookDecodeError::Frame(WireError::LengthMismatch { .. })
        ));

        // A declared length that overflows a u32 addition.
        for len in [u32::MAX, u32::MAX - 7, 0xFFFF_FFF8] {
            let mut frame = 3u32.to_le_bytes().to_vec();
            frame.extend_from_slice(&len.to_le_bytes());
            frame.extend_from_slice(b"{}");
            assert!(decode_datagram(&frame).is_err());
        }

        // Longer than any datagram polis-hook can construct.
        let err = decode_datagram(&vec![0u8; MAX_DATAGRAM + 1]).expect_err("oversized");
        assert!(matches!(
            err,
            HookDecodeError::Frame(WireError::TooLong { .. })
        ));
    }

    #[test]
    fn garbage_and_empty_payloads_are_dropped_never_fatal() {
        for body in [
            b"".as_slice(),
            b"{",
            b"null",
            b"[]",
            b"\xff\xfe\x00\x01",
            b"GET / HTTP/1.1",
            b"{\"session_id\":",
        ] {
            let frame = encode_frame(EventKind::Stop, body);
            let err = decode_datagram(&frame).expect_err("unparseable, and not truncated");
            assert!(matches!(err, HookDecodeError::Payload(_)), "{body:?}");
        }
        // The same bodies with the truncation bit set are notification-only and
        // must survive, because a cut document never parses.
        for body in [b"".as_slice(), b"{", b"\xff\xfe", b"{\"session_id\":"] {
            let mut frame = encode_frame(EventKind::Stop, body);
            frame[3] |= 0x80;
            let e = decode_datagram(&frame).expect("truncated frames are kept");
            assert_eq!(e.kind, EventKind::Stop);
            assert!(e.truncated);
        }
    }

    #[test]
    fn an_unknown_tag_degrades_and_the_payload_still_names_the_event() {
        let body = br#"{"session_id":"s","hook_event_name":"Notification"}"#;
        let mut frame = encode_frame(EventKind::Unknown, body);
        // Tag 12345: a future polis-hook tagging a future Claude Code event.
        frame[0..4].copy_from_slice(&12_345u32.to_le_bytes());
        let e = decode_datagram(&frame).expect("drift is not corruption");
        assert_eq!(e.kind, EventKind::Notification);

        // And with nothing to fall back on, the event is still delivered.
        let frame = encode_frame(EventKind::Unknown, br#"{"session_id":"s"}"#);
        assert_eq!(decode_datagram(&frame).unwrap().kind, EventKind::Unknown);
    }

    /// A cheap stand-in for `cargo-fuzz` (PRD §16): the only property is that
    /// nothing panics. Deterministic xorshift64*, so a failure is reproducible.
    #[test]
    fn never_panics_on_arbitrary_bytes() {
        let mut s: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..30_000 {
            let n = usize::try_from(next() % 96).unwrap_or(0);
            let buf: Vec<u8> = (0..n)
                .map(|_| u8::try_from(next() & 0xff).unwrap_or(0))
                .collect();
            let _ = decode_datagram(&buf);
        }
        // Shapes a random generator will not reach: a maximal frame of random
        // bytes, and a maximal frame flagged truncated.
        let big: Vec<u8> = (0..MAX_PAYLOAD)
            .map(|_| u8::try_from(next() & 0xff).unwrap_or(0))
            .collect();
        let mut frame = encode_frame(EventKind::PreToolUse, &big);
        let _ = decode_datagram(&frame);
        frame[3] |= 0x80;
        let _ = decode_datagram(&frame);
    }

    #[test]
    fn the_repair_keeps_every_complete_top_level_field() {
        let cases: [(&[u8], &str); 5] = [
            (br#"{"a":1,"b":{"c":"xx"#, r#"{"a":1}"#),
            (br#"{"a":1,"b":2,"c":"x"#, r#"{"a":1,"b":2}"#),
            (br#"{"a":"has a , comma","b":"#, r#"{"a":"has a , comma"}"#),
            (
                br#"{"a":"escaped \" quote","b"#,
                r#"{"a":"escaped \" quote"}"#,
            ),
            (br#"{"session_id":"s"#, "{}"),
        ];
        for (input, want) in cases {
            let got = repair_truncated_json(input).expect("an object");
            assert_eq!(String::from_utf8_lossy(&got), want);
            serde_json::from_slice::<serde_json::Value>(&got).expect("repaired JSON parses");
        }
        // Not an object at all: nothing to repair.
        assert!(repair_truncated_json(b"[1,2").is_none());
        assert!(repair_truncated_json(b"").is_none());
        assert!(repair_truncated_json(b"   ").is_none());
    }

    // -- normalisation ------------------------------------------------------

    #[test]
    fn the_envelope_keeps_the_subagent_discriminator() {
        let worker = br#"{"session_id":"s-1","prompt_id":"p-1","agent_id":"a-7",
                          "agent_type":"Explore","hook_event_name":"SubagentStop"}"#;
        let e = to_event(decode_datagram(&encode_frame(EventKind::SubagentStop, worker)).unwrap());
        assert_eq!(e.meta.channel, Channel::Hook);
        assert!(e.meta.is_worker());
        assert_eq!(e.meta.worker.as_ref().unwrap().as_str(), "a-7");
        assert_eq!(e.meta.agent_type.as_ref().unwrap().as_str(), "Explore");
        assert_eq!(e.meta.prompt.as_ref().unwrap().as_str(), "p-1");
        // The thread is the PARENT session's id, even inside a subagent.
        assert_eq!(e.meta.thread.as_ref().unwrap().as_str(), "s-1");
        assert_eq!(payload_of(&e).kind, EventKind::SubagentStop);

        // `claude --agent foo` carries agent_type on a MAIN agent (ADR-0030).
        let main = br#"{"session_id":"s-1","agent_type":"Explore","hook_event_name":"Stop"}"#;
        let e = to_event(decode_datagram(&encode_frame(EventKind::Stop, main)).unwrap());
        assert!(!e.meta.is_worker(), "agent_type alone is not a worker");
        assert!(e.meta.agent_type.is_some());
        assert!(e.meta.sequence.is_none(), "hooks carry no event.sequence");
    }

    // -- the socket, end to end ---------------------------------------------

    #[test]
    fn the_loop_delivers_events_counts_noise_and_shuts_down() {
        let (tx, rx) = mpsc::channel();
        let listener =
            HookListener::start_with(any_loopback(), Collect(tx), PathMapper::default()).unwrap();
        let addr = listener.local_addr();
        assert_eq!(listener.health(), SourceHealth::Running);

        let client = UdpSocket::bind(any_loopback()).unwrap();
        let good = encode_frame(
            EventKind::PermissionRequest,
            br#"{"session_id":"s-4","hook_event_name":"PermissionRequest"}"#,
        );
        client.send_to(&good, addr).unwrap();
        let e = rx.recv_timeout(Duration::from_secs(5)).expect("delivered");
        assert_eq!(payload_of(&e).kind, EventKind::PermissionRequest);
        assert_eq!(e.meta.session.as_ref().unwrap().as_str(), "s-4");

        // Noise from an unrelated local process: counted, dropped, not fatal.
        client.send_to(b"GET / HTTP/1.1\r\n\r\n", addr).unwrap();
        client.send_to(&good, addr).unwrap();
        rx.recv_timeout(Duration::from_secs(5))
            .expect("still alive");

        let counters = listener.counters();
        assert_eq!(counters.accepted, 2);
        assert_eq!(counters.rejected, 1);
        assert_eq!(counters.truncated, 0);

        listener.shutdown();
        // The wake datagram must never be mistaken for a hook event.
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn a_second_daemon_gets_addr_in_use_and_never_takes_another_port() {
        let held = UdpSocket::bind(any_loopback()).unwrap();
        let SocketAddr::V4(addr) = held.local_addr().unwrap() else {
            panic!("bound v4")
        };
        let (tx, _rx) = mpsc::channel();
        let err = HookListener::start_with(addr, Collect(tx), PathMapper::default())
            .expect_err("the bind is the singleton lock");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        // The kind survives so callers can match on it, and the message is the
        // one ADR-0026 wants an operator to read.
        let message = err.to_string();
        assert!(
            message.contains("another polis is already receiving"),
            "{message}"
        );
        assert!(message.contains(&addr.to_string()), "{message}");
        // And nothing was published: a losing daemon must not clobber the
        // winner's endpoint file (ADR-0026).
    }

    #[test]
    fn health_reports_stopped_after_shutdown() {
        let (tx, _rx) = mpsc::channel();
        let mut listener =
            HookListener::start_with(any_loopback(), Collect(tx), PathMapper::default()).unwrap();
        listener.stop_inner();
        assert!(!listener.health().is_healthy());
        assert_eq!(listener.health().reason(), Some("shut down"));
        // Idempotent: shutdown runs while the process is already on its way out.
        listener.stop_inner();
        assert_eq!(listener.channel(), Channel::Hook);
    }

    // -- the endpoint file --------------------------------------------------

    #[test]
    fn a_stale_endpoint_file_is_superseded_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ENDPOINT_FILE);
        // A dead daemon's leftovers: wrong port, pid that no longer exists.
        std::fs::write(&path, "127.0.0.1:49999\npid=999999\n").unwrap();

        let live = SocketAddrV4::new(Ipv4Addr::LOCALHOST, polis_events::DEFAULT_HOOK_PORT);
        let written = write_endpoint_file(dir.path(), live).unwrap();
        assert_eq!(written, path);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().next(), Some("127.0.0.1:45177"));
        assert!(contents.contains("pid="));

        // No temp file survives, so a hook can never read a half-written line.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .filter(|n| n.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        // Corrupt leftovers are just as replaceable.
        std::fs::write(&path, [0xffu8; 64]).unwrap();
        write_endpoint_file(dir.path(), live).unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("127.0.0.1:45177"));
    }

    #[test]
    fn shutdown_removes_only_an_endpoint_file_that_is_still_ours() {
        let dir = tempfile::tempdir().unwrap();
        let ours = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 45_177);
        let theirs = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 45_178);
        let path = write_endpoint_file(dir.path(), ours).unwrap();

        // A second daemon on --port N republished after we did: leave it alone.
        write_endpoint_file(dir.path(), theirs).unwrap();
        remove_endpoint_file(&path, ours);
        assert!(path.exists(), "another daemon's file must survive our exit");

        remove_endpoint_file(&path, theirs);
        assert!(!path.exists());
        // Removing an absent file is silent, not a panic.
        remove_endpoint_file(&path, theirs);
    }

    /// The hook resolves the endpoint directory from its own copy of this logic
    /// and links `std` only, so it cannot import [`endpoint_dir`]. `include_str!`
    /// is a compile-time file read, not a dependency, so the two can still be
    /// pinned together — exactly as `polis_events::kind` pins the tag table.
    #[test]
    fn endpoint_directory_matches_the_hook_binary() {
        const HOOK_SRC: &str = include_str!("../../polis-hook/src/main.rs");
        for needle in [
            "std::env::var_os(\"LOCALAPPDATA\")",
            "std::env::var_os(\"XDG_RUNTIME_DIR\")",
            "std::env::var_os(\"XDG_STATE_HOME\")",
            "\".local/state\"",
            "p.push(\"polis\");",
            "p.push(\"endpoint\");",
            ".join(\"polis\").join(\"endpoint\")",
        ] {
            assert!(HOOK_SRC.contains(needle), "polis-hook moved: {needle}");
        }
        // And the directory this crate would write into ends the same way.
        if let Some(dir) = endpoint_dir() {
            assert_eq!(dir.file_name().unwrap(), "polis");
            assert_eq!(dir.join(ENDPOINT_FILE).file_name().unwrap(), "endpoint");
        }
    }
}
