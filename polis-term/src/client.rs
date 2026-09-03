//! The window's connection to the session daemon.
//!
//! One TCP stream, one reader thread, and one `Emulator` per pane. The reader
//! thread does three things per [`Message::Output`], in this order:
//!
//! 1. feeds the bytes to that pane's emulator — **on this thread**, never on the
//!    UI's, so an adversarially large burst cannot land inside a frame;
//! 2. sends any [`PaneSignal::Reply`] straight back down the same socket,
//!    because Claude Code is blocked waiting for it;
//! 3. wakes the window, and only if the generation actually moved.
//!
//! Everything the UI needs is then either a lock-free `AtomicU64` read (has this
//! pane changed?) or one short lock (give me the screen).
//!
//! # What the UI thread must do, and what it must not
//!
//! It must not hold [`PaneState::emulator`] across a paint of anything else, and
//! it must never touch the socket except through [`SessionClient::send`], which
//! does not block. Everything else — attaching, replaying history, reconnecting
//! — happens here.

use std::collections::BTreeMap;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};

use crate::emu::{Emulator, PaneSignal};
use crate::proto::{Call, Message, PaneId, PaneInfo, Reply, Request, PROTOCOL};
use crate::transport::{Endpoint, Frames, Wire};

/// How long a request waits before the caller is told the daemon is not talking.
///
/// Long enough for a cold `claude` spawn on a slow disk, short enough that a
/// wedged daemon shows up as a message rather than a frozen window.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// What the window is told about, as it happens.
#[derive(Debug, Clone)]
pub enum ClientEvent {
    /// A pane exists. Its [`PaneState`] is already in place.
    Opened(Box<PaneInfo>),
    /// A pane's child exited. The pane and its last screen remain.
    Exited(PaneId, Option<i32>),
    /// A pane is gone.
    Closed(PaneId),
    /// A pane's terminal asked for something the window owns.
    ///
    /// [`PaneSignal::Reply`] never arrives here — it is answered on the reader
    /// thread, before this is sent, because the child is blocked on it.
    Signal(PaneId, PaneSignal),
    /// The connection ended. Everything after this is stale.
    Disconnected(String),
}

/// One pane, as the window holds it.
///
/// Shared between the reader thread, which writes, and the frame, which reads.
#[derive(Debug)]
pub struct PaneState {
    /// Which pane.
    pub id: PaneId,
    /// The parsed screen. Locked briefly by the frame, longer by the reader.
    pub emulator: Mutex<Emulator>,
    /// Bumped by the emulator on every change.
    ///
    /// Read without the lock, so "is this pane a reason to repaint?" is one
    /// atomic load per pane per frame.
    pub generation: Arc<AtomicU64>,
    /// The daemon's view of the pane, refreshed as messages arrive.
    pub info: Mutex<PaneInfo>,
    /// How far into the byte log this pane has been fed.
    ///
    /// The `since` a reconnect passes. Held here rather than in the app so a
    /// reconnect cannot lose a screen by forgetting to carry it.
    pub consumed: AtomicU64,
}

impl PaneState {
    fn new(info: PaneInfo) -> Arc<Self> {
        let emulator = Emulator::new(info.rows, info.cols);
        let generation = emulator.generation_handle();
        Arc::new(Self {
            id: info.pane,
            emulator: Mutex::new(emulator),
            generation,
            info: Mutex::new(info),
            consumed: AtomicU64::new(0),
        })
    }

    /// The generation, without taking the lock.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

/// State the reader thread and the API share.
struct Shared {
    panes: Mutex<BTreeMap<PaneId, Arc<PaneState>>>,
    pending: Mutex<BTreeMap<u64, Sender<Result<Reply, String>>>>,
    events: Sender<ClientEvent>,
    wire: Mutex<Wire<TcpStream>>,
    wake: Box<dyn Fn() + Send + Sync>,
    live: AtomicBool,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("live", &self.live.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Shared {
    fn send(&self, call: &Call) -> Result<(), String> {
        self.wire
            .lock()
            .map_err(|_| "the connection is poisoned".to_owned())?
            .send(call)
            .map_err(|error| format!("the session daemon stopped listening: {error}"))
    }
}

/// A connection to the session daemon.
#[derive(Debug)]
pub struct SessionClient {
    shared: Arc<Shared>,
    events: Receiver<ClientEvent>,
    next_id: AtomicU64,
    reader: Option<std::thread::JoinHandle<()>>,
    stream: TcpStream,
    /// The daemon's version, for the status bar.
    pub daemon_version: String,
    /// The daemon's process id, for `polis doctor`.
    pub daemon_pid: u32,
}

impl SessionClient {
    /// Connects, says hello, and starts following.
    ///
    /// `wake` is called whenever a pane's screen changed — in the window it is
    /// `ctx.request_repaint()`, and it is called **only** when the generation
    /// moved, which is what keeps a quiet pane out of PRD §13.1's idle budget.
    ///
    /// # Errors
    ///
    /// When the socket refuses, when the token is wrong, or when the daemon
    /// speaks a different [`PROTOCOL`] — the last of which is refused loudly and
    /// by name, because a daemon left running across an upgrade is the expected
    /// case, not the exotic one.
    pub fn connect(
        endpoint: &Endpoint,
        client: &str,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, String> {
        if endpoint.protocol != PROTOCOL {
            return Err(format!(
                "the session daemon speaks protocol {} and this window speaks {PROTOCOL} — \
                 close the window, run `polis serve --stop`, and start again",
                endpoint.protocol
            ));
        }
        let stream = crate::transport::connect(endpoint)
            .map_err(|error| format!("no session daemon at {}: {error}", endpoint.addr))?;
        let reading = stream
            .try_clone()
            .map_err(|error| format!("could not split the connection: {error}"))?;

        let (events_tx, events) = crossbeam_channel::unbounded();
        let shared = Arc::new(Shared {
            panes: Mutex::new(BTreeMap::new()),
            pending: Mutex::new(BTreeMap::new()),
            events: events_tx,
            wire: Mutex::new(Wire::new(
                stream
                    .try_clone()
                    .map_err(|error| format!("could not split the connection: {error}"))?,
            )),
            wake: Box::new(wake),
            live: AtomicBool::new(true),
        });

        let thread_shared = Arc::clone(&shared);
        let reader = std::thread::Builder::new()
            .name("polis-term-client".to_owned())
            .spawn(move || follow(&thread_shared, reading))
            .map_err(|error| format!("could not start the connection thread: {error}"))?;

        let mut this = Self {
            shared,
            events,
            next_id: AtomicU64::new(1),
            reader: Some(reader),
            stream,
            daemon_version: String::new(),
            daemon_pid: 0,
        };

        match this.call(Request::Hello {
            protocol: PROTOCOL,
            client: client.to_owned(),
            token: endpoint.token.clone(),
        })? {
            Reply::Hello {
                protocol,
                version,
                pid,
            } => {
                if protocol != PROTOCOL {
                    return Err(format!(
                        "the session daemon speaks protocol {protocol} and this window \
                         speaks {PROTOCOL}"
                    ));
                }
                this.daemon_version = version;
                this.daemon_pid = pid;
            }
            other => return Err(format!("the daemon answered hello with {other:?}")),
        }
        Ok(this)
    }

    /// Sends a request and waits for its answer.
    ///
    /// # Errors
    ///
    /// When the daemon refuses, when the connection drops, or after
    /// [`CALL_TIMEOUT`].
    pub fn call(&self, request: Request) -> Result<Reply, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.shared
            .pending
            .lock()
            .map_err(|_| "the connection is poisoned".to_owned())?
            .insert(id, tx);
        if let Err(error) = self.shared.send(&Call { id, request }) {
            let _ = self.shared.pending.lock().map(|mut p| p.remove(&id));
            return Err(error);
        }
        let Ok(result) = rx.recv_timeout(CALL_TIMEOUT) else {
            let _ = self.shared.pending.lock().map(|mut p| p.remove(&id));
            return Err("the session daemon did not answer".to_owned());
        };
        result
    }

    /// Sends a request and does not wait. For keystrokes and resizes.
    ///
    /// # Errors
    ///
    /// When the connection has dropped.
    pub fn send(&self, request: Request) -> Result<(), String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.shared.send(&Call { id, request })
    }

    /// Everything that has happened since the last look. Never blocks.
    #[must_use]
    pub fn events(&self) -> &Receiver<ClientEvent> {
        &self.events
    }

    /// One pane, if the daemon has told us about it.
    #[must_use]
    pub fn pane(&self, id: PaneId) -> Option<Arc<PaneState>> {
        self.shared.panes.lock().ok()?.get(&id).cloned()
    }

    /// Every pane, in id order.
    #[must_use]
    pub fn panes(&self) -> Vec<Arc<PaneState>> {
        self.shared
            .panes
            .lock()
            .map(|panes| panes.values().cloned().collect())
            .unwrap_or_default()
    }

    /// True until the reader thread has seen the connection end.
    #[must_use]
    pub fn connected(&self) -> bool {
        self.shared.live.load(Ordering::Acquire)
    }

    /// Adopts a pane the daemon already had, replaying its history.
    ///
    /// This is reattach: the pane's whole byte log is replayed through a fresh
    /// parser, so the screen the operator sees is the screen that would have
    /// been there if the window had never closed — scrollback included. There is
    /// nothing to serialise, because the bytes *are* the state.
    ///
    /// # Errors
    ///
    /// When the daemon refuses the attach.
    pub fn adopt(&self, info: PaneInfo) -> Result<Arc<PaneState>, String> {
        let pane = info.pane;
        let state = self.remember(info);
        let since = state.consumed.load(Ordering::Acquire);
        self.call(Request::Attach { pane, since })?;
        Ok(state)
    }

    /// Inserts a pane, or returns the one already held.
    fn remember(&self, info: PaneInfo) -> Arc<PaneState> {
        remember(&self.shared, info)
    }
}

impl Drop for SessionClient {
    fn drop(&mut self) {
        // Shutting the socket down is what unblocks the reader's `read_line`;
        // dropping the last handle would not, because the thread holds a clone.
        self.shared.live.store(false, Ordering::Release);
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Inserts a pane, or updates the one already held.
fn remember(shared: &Arc<Shared>, info: PaneInfo) -> Arc<PaneState> {
    let mut panes = match shared.panes.lock() {
        Ok(panes) => panes,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(existing) = panes.get(&info.pane) {
        if let Ok(mut held) = existing.info.lock() {
            *held = info;
        }
        return Arc::clone(existing);
    }
    let state = PaneState::new(info);
    panes.insert(state.id, Arc::clone(&state));
    state
}

/// The reader thread: parse, answer, wake.
fn follow(shared: &Arc<Shared>, stream: TcpStream) {
    let mut frames = Frames::new(stream);
    let reason = loop {
        match frames.recv::<Message>() {
            Ok(Some(message)) => handle(shared, message),
            Ok(None) => break "the session daemon closed the connection".to_owned(),
            Err(error) => break format!("the session daemon sent something unreadable: {error}"),
        }
    };
    shared.live.store(false, Ordering::Release);
    // Anyone still waiting on a reply is waiting forever otherwise.
    if let Ok(mut pending) = shared.pending.lock() {
        for (_, waiter) in std::mem::take(&mut *pending) {
            let _ = waiter.send(Err(reason.clone()));
        }
    }
    let _ = shared.events.send(ClientEvent::Disconnected(reason));
    (shared.wake)();
}

fn handle(shared: &Arc<Shared>, message: Message) {
    match message {
        Message::Reply { id, result } => answer(shared, id, Ok(result)),
        Message::Failed { id, error } => answer(shared, id, Err(error)),
        Message::Output { pane, offset, data } => output(shared, pane, offset, &data),
        Message::Opened { info } => {
            let state = remember(shared, (*info).clone());
            let _ = state;
            let _ = shared.events.send(ClientEvent::Opened(info));
            (shared.wake)();
        }
        Message::Exit { pane, code } => {
            if let Some(state) = lookup(shared, pane) {
                if let Ok(mut info) = state.info.lock() {
                    info.exit = code;
                }
            }
            let _ = shared.events.send(ClientEvent::Exited(pane, code));
            (shared.wake)();
        }
        Message::Closed { pane } => {
            if let Ok(mut panes) = shared.panes.lock() {
                panes.remove(&pane);
            }
            let _ = shared.events.send(ClientEvent::Closed(pane));
            (shared.wake)();
        }
        Message::GoingAway { reason } => {
            let _ = shared.events.send(ClientEvent::Disconnected(reason));
            (shared.wake)();
        }
    }
}

fn answer(shared: &Arc<Shared>, id: u64, result: Result<Reply, String>) {
    let waiter = shared.pending.lock().ok().and_then(|mut p| p.remove(&id));
    if let Some(waiter) = waiter {
        let _ = waiter.send(result);
    }
}

/// The hot path: parse the bytes, answer what the terminal asks, wake the window.
fn output(shared: &Arc<Shared>, pane: PaneId, offset: u64, data: &[u8]) {
    let Some(state) = lookup(shared, pane) else {
        // Output for a pane we were never told about. The daemon always sends
        // `Opened` first, so this is a message from a race we lost; dropping it
        // would tear the screen, so ask for the pane from the start instead.
        let _ = shared.send(&Call {
            id: 0,
            request: Request::Attach { pane, since: 0 },
        });
        return;
    };

    let (before, after, signals) = {
        let mut emulator = match state.emulator.lock() {
            Ok(emulator) => emulator,
            Err(poisoned) => poisoned.into_inner(),
        };
        let before = state.generation();
        emulator.feed(data);
        let signals = emulator.take_signals();
        (before, state.generation(), signals)
    };
    state
        .consumed
        .store(offset + data.len() as u64, Ordering::Release);

    for signal in signals {
        match signal {
            // Answered here, on this thread, immediately. The child is blocked.
            PaneSignal::Reply(bytes) => {
                let _ = shared.send(&Call {
                    id: 0,
                    request: Request::Write { pane, data: bytes },
                });
            }
            PaneSignal::Title(ref title) => {
                if let Ok(mut info) = state.info.lock() {
                    info.title.clone_from(title);
                }
                let _ = shared.events.send(ClientEvent::Signal(pane, signal));
            }
            other => {
                let _ = shared.events.send(ClientEvent::Signal(pane, other));
            }
        }
    }

    // The whole idle budget in one line: a pane that produced nothing visible
    // is not a reason to draw a frame.
    if after != before {
        (shared.wake)();
    }
}

fn lookup(shared: &Arc<Shared>, pane: PaneId) -> Option<Arc<PaneState>> {
    shared.panes.lock().ok()?.get(&pane).cloned()
}
