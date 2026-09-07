//! The pane registry — every pty the daemon owns, and who is watching it.
//!
//! # One lock, and nothing held across a spawn
//!
//! Everything lives under a single `Mutex<Registry>`. The obvious deadlock is
//! that [`Panes::open`] would hold the lock while `PtyHost::spawn` starts a
//! thread that immediately calls back with output, which wants the same lock. It
//! cannot happen here, for two reasons that are both deliberate:
//!
//! 1. Pty output does not call back into the registry at all. It goes down a
//!    channel to one **pump thread**, which is the only thing that ever turns a
//!    [`PtyEvent`] into registry work.
//! 2. The pane's record — including its byte log — is inserted **before** the
//!    child is spawned, so output that arrives while `spawn` is still returning
//!    has somewhere to land.
//!
//! # Backpressure is solved by the byte log, not by flow control
//!
//! Each subscriber tracks how far into each pane's log it has been sent. When
//! its queue is full the daemon simply *does not send* and leaves the mark where
//! it is; the next event reads from the log and catches the client up in one
//! coalesced chunk. A slow client falls behind and then catches up, and never
//! loses a byte or stalls a pty.
//!
//! That is the same outcome tmux reaches with `pause-after`/`%continue`, without
//! the protocol: keeping the history is what makes it free.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use polis_term::log::ByteLog;
use polis_term::proto::{Message, OpenPane, PaneId, PaneInfo};
use polis_term::pty::{PtyEvent, PtyHost, PtySpec};

/// How many messages a client may fall behind before the daemon stops enqueuing
/// and starts coalescing through the log instead.
///
/// Small on purpose: a large queue only delays the moment coalescing starts, and
/// coalescing is cheaper than the queue.
const CLIENT_QUEUE: usize = 256;

/// Which connection. Not on the wire — clients never learn about each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ClientId(pub(crate) u64);

/// One connected client's outbox and what it is following.
struct Subscriber {
    outbox: Sender<Message>,
    /// Per pane, the offset this client has been sent up to.
    following: BTreeMap<PaneId, u64>,
}

/// One pane.
struct Pane {
    info: PaneInfo,
    /// `None` only in the instant between the record being inserted and the
    /// child being spawned.
    host: Option<PtyHost>,
    log: ByteLog,
}

/// Everything the daemon owns, under one lock.
struct Registry {
    next_pane: u32,
    panes: BTreeMap<PaneId, Pane>,
    clients: BTreeMap<ClientId, Subscriber>,
    log_dir: PathBuf,
}

/// The daemon's panes.
#[derive(Debug)]
pub(crate) struct Panes {
    inner: Mutex<Registry>,
    events: Sender<(PaneId, PtyEvent)>,
    next_client: AtomicU64,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("panes", &self.panes.len())
            .field("clients", &self.clients.len())
            .finish_non_exhaustive()
    }
}

impl Panes {
    /// Builds the registry and starts its pump thread.
    ///
    /// `log_dir` holds one file per pane. A directory that cannot be written
    /// costs reattach and nothing else — see [`ByteLog::discarding`].
    ///
    /// # Errors
    ///
    /// When the pump thread cannot be started.
    pub(crate) fn start(log_dir: PathBuf) -> Result<Arc<Self>, String> {
        let (events, inbox) = crossbeam_channel::unbounded();
        let panes = Arc::new(Self {
            inner: Mutex::new(Registry {
                next_pane: 1,
                panes: BTreeMap::new(),
                clients: BTreeMap::new(),
                log_dir,
            }),
            events,
            next_client: AtomicU64::new(1),
        });
        let pump = Arc::clone(&panes);
        std::thread::Builder::new()
            .name("polis-sessiond-pump".to_owned())
            .spawn(move || pump_events(&pump, &inbox))
            .map_err(|error| format!("could not start the pane pump: {error}"))?;
        Ok(panes)
    }

    fn lock(&self) -> MutexGuard<'_, Registry> {
        // A poisoned registry is still a registry: a panic in one connection
        // must not take every running agent down with it.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registers a connection, returning its id and the queue it should drain.
    pub(crate) fn connect(&self) -> (ClientId, Receiver<Message>) {
        let id = ClientId(self.next_client.fetch_add(1, Ordering::Relaxed));
        let (outbox, inbox) = crossbeam_channel::bounded(CLIENT_QUEUE);
        self.lock().clients.insert(
            id,
            Subscriber {
                outbox,
                following: BTreeMap::new(),
            },
        );
        (id, inbox)
    }

    /// One connection's outbox, for the replies its own thread sends.
    ///
    /// Replies go down the same queue as events so that a client sees them in
    /// the order they happened — an `Opened` that overtook its own reply would
    /// arrive for a pane the client does not yet know it asked for.
    pub(crate) fn outbox(&self, client: ClientId) -> Option<Sender<Message>> {
        self.lock()
            .clients
            .get(&client)
            .map(|subscriber| subscriber.outbox.clone())
    }

    /// Forgets a connection. Its panes keep running — that is the whole point.
    pub(crate) fn disconnect(&self, client: ClientId) {
        self.lock().clients.remove(&client);
    }

    /// Every pane, in id order.
    pub(crate) fn list(&self) -> Vec<PaneInfo> {
        self.lock()
            .panes
            .values()
            .map(|pane| pane.info.clone())
            .collect()
    }

    /// How many panes exist, live or exited.
    pub(crate) fn count(&self) -> usize {
        self.lock().panes.len()
    }

    /// How many connections are open.
    pub(crate) fn client_count(&self) -> usize {
        self.lock().clients.len()
    }

    /// Starts a child on a new pty.
    ///
    /// # Errors
    ///
    /// When the program is not on `PATH` or the platform refuses a pty. The
    /// half-inserted pane is removed before returning, so a failed open leaves
    /// nothing behind.
    pub(crate) fn open(self: &Arc<Self>, request: &OpenPane) -> Result<PaneInfo, String> {
        let (pane, log) = {
            let mut registry = self.lock();
            let pane = PaneId(registry.next_pane);
            registry.next_pane += 1;
            let log = ByteLog::create(&registry.log_dir, pane).unwrap_or_else(|error| {
                tracing::warn!("pane {pane} gets no history: {error}");
                ByteLog::discarding()
            });
            (pane, log)
        };

        let info = PaneInfo {
            pane,
            session_id: request.session_id.clone(),
            program: request.program.clone(),
            args: request.args.clone(),
            cwd: request.cwd.clone(),
            rows: request.rows,
            cols: request.cols,
            pid: None,
            title: request.program.clone(),
            bytes: 0,
            started_unix_ms: now_unix_ms(),
            exit: None,
        };
        self.lock().panes.insert(
            pane,
            Pane {
                info: info.clone(),
                host: None,
                log,
            },
        );

        let spec = PtySpec {
            program: request.program.clone(),
            args: request.args.clone(),
            cwd: Some(request.cwd.clone()),
            env: request.env.clone(),
            rows: request.rows,
            cols: request.cols,
        };
        let events = self.events.clone();
        let host = match PtyHost::spawn(&spec, &pane.0.to_string(), move |event| {
            let _ = events.send((pane, event));
        }) {
            Ok(host) => host,
            Err(error) => {
                let mut registry = self.lock();
                if let Some(mut dead) = registry.panes.remove(&pane) {
                    dead.log.remove();
                }
                return Err(error);
            }
        };

        let mut registry = self.lock();
        let Some(entry) = registry.panes.get_mut(&pane) else {
            // Closed while spawning. `host` drops here, which ends the child.
            return Err("the pane was closed before it started".to_owned());
        };
        entry.info.pid = host.pid();
        entry.host = Some(host);
        let info = entry.info.clone();
        registry.broadcast(&Message::Opened {
            info: Box::new(info.clone()),
        });
        Ok(info)
    }

    /// Sends bytes to a pane's child.
    ///
    /// # Errors
    ///
    /// When there is no such pane, or its child has already exited — a
    /// keystroke silently going nowhere is worse than one that says so.
    pub(crate) fn write(&self, pane: PaneId, data: Vec<u8>) -> Result<(), String> {
        let registry = self.lock();
        let entry = registry.panes.get(&pane).ok_or_else(|| unknown(pane))?;
        let host = entry
            .host
            .as_ref()
            .ok_or_else(|| format!("{pane} has no running child"))?;
        host.write(data);
        Ok(())
    }

    /// Tells a pane's pty its new size.
    ///
    /// # Errors
    ///
    /// When there is no such pane.
    pub(crate) fn resize(&self, pane: PaneId, rows: u16, cols: u16) -> Result<(), String> {
        let mut registry = self.lock();
        let entry = registry.panes.get_mut(&pane).ok_or_else(|| unknown(pane))?;
        if (entry.info.rows, entry.info.cols) == (rows, cols) {
            return Ok(());
        }
        entry.info.rows = rows;
        entry.info.cols = cols;
        if let Some(host) = entry.host.as_ref() {
            host.resize(rows, cols);
        }
        Ok(())
    }

    /// Starts sending a pane's output to `client`, from `since` onwards.
    ///
    /// # Errors
    ///
    /// When there is no such pane.
    pub(crate) fn attach(&self, client: ClientId, pane: PaneId, since: u64) -> Result<(), String> {
        let mut registry = self.lock();
        if !registry.panes.contains_key(&pane) {
            return Err(unknown(pane));
        }
        if let Some(subscriber) = registry.clients.get_mut(&client) {
            subscriber.following.insert(pane, since);
        }
        registry.catch_up(client, pane);
        Ok(())
    }

    /// Stops sending a pane's output to `client`. The child keeps running.
    pub(crate) fn detach(&self, client: ClientId, pane: PaneId) {
        if let Some(subscriber) = self.lock().clients.get_mut(&client) {
            subscriber.following.remove(&pane);
        }
    }

    /// Ends a pane's child and forgets it.
    ///
    /// # Errors
    ///
    /// When there is no such pane.
    pub(crate) fn close(&self, pane: PaneId) -> Result<(), String> {
        let removed = {
            let mut registry = self.lock();
            registry.panes.remove(&pane)
        };
        let Some(mut entry) = removed else {
            return Err(unknown(pane));
        };
        // Outside the lock: shutting a pty down waits on a thread, and no other
        // pane should have to wait behind it.
        if let Some(mut host) = entry.host.take() {
            host.shutdown();
        }
        entry.log.remove();
        let mut registry = self.lock();
        for subscriber in registry.clients.values_mut() {
            subscriber.following.remove(&pane);
        }
        registry.broadcast(&Message::Closed { pane });
        Ok(())
    }

    /// Ends every pane. Called on the daemon's way out.
    pub(crate) fn close_all(&self, reason: &str) {
        let ids: Vec<PaneId> = self.lock().panes.keys().copied().collect();
        for pane in ids {
            let _ = self.close(pane);
        }
        self.lock().broadcast(&Message::GoingAway {
            reason: reason.to_owned(),
        });
    }
}

impl Registry {
    /// Sends one message to every connection, dropping it for any that is full.
    ///
    /// Lifecycle messages are small and rare; a client that cannot take one is a
    /// client that is about to be disconnected anyway.
    fn broadcast(&mut self, message: &Message) {
        for subscriber in self.clients.values() {
            let _ = subscriber.outbox.try_send(message.clone());
        }
    }

    /// Sends `client` everything it has not seen of `pane`.
    ///
    /// Returns without changing the mark when the client's queue is full, which
    /// is the whole of the backpressure policy: the log already holds the bytes,
    /// so the next call coalesces them into one larger message.
    fn catch_up(&mut self, client: ClientId, pane: PaneId) {
        let Some(sent) = self
            .clients
            .get(&client)
            .and_then(|s| s.following.get(&pane).copied())
        else {
            return;
        };
        let Some(entry) = self.panes.get_mut(&pane) else {
            return;
        };
        if sent >= entry.log.len() {
            return;
        }
        let Ok((offset, data)) = entry.log.read_from(sent) else {
            return;
        };
        if data.is_empty() {
            return;
        }
        let end = offset + data.len() as u64;
        let Some(subscriber) = self.clients.get_mut(&client) else {
            return;
        };
        match subscriber
            .outbox
            .try_send(Message::Output { pane, offset, data })
        {
            Ok(()) => {
                subscriber.following.insert(pane, end);
            }
            Err(TrySendError::Full(_)) => {
                // Leave the mark. The next event coalesces.
                tracing::debug!("client {client:?} is behind on {pane}");
            }
            Err(TrySendError::Disconnected(_)) => {
                subscriber.following.remove(&pane);
            }
        }
    }

    /// Every client following `pane`.
    fn followers(&self, pane: PaneId) -> Vec<ClientId> {
        self.clients
            .iter()
            .filter(|(_, s)| s.following.contains_key(&pane))
            .map(|(id, _)| *id)
            .collect()
    }
}

/// The pump thread: the only place a [`PtyEvent`] becomes registry work.
fn pump_events(panes: &Arc<Panes>, inbox: &Receiver<(PaneId, PtyEvent)>) {
    while let Ok((pane, event)) = inbox.recv() {
        let mut registry = panes.lock();
        match event {
            PtyEvent::Output(bytes) => {
                let Some(entry) = registry.panes.get_mut(&pane) else {
                    continue;
                };
                entry.log.append(&bytes);
                entry.info.bytes = entry.log.len();
            }
            PtyEvent::Exited(code) => {
                if let Some(entry) = registry.panes.get_mut(&pane) {
                    entry.info.exit = Some(code.unwrap_or(-1));
                    entry.host = None;
                }
                registry.broadcast(&Message::Exit { pane, code });
            }
        }
        for client in registry.followers(pane) {
            registry.catch_up(client, pane);
        }
    }
}

fn unknown(pane: PaneId) -> String {
    format!("there is no pane {pane}")
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn echoing(text: &str) -> OpenPane {
        let (program, args) = if cfg!(windows) {
            (
                "cmd".to_owned(),
                vec!["/c".to_owned(), format!("echo {text}")],
            )
        } else {
            (
                "sh".to_owned(),
                vec!["-c".to_owned(), format!("echo {text}")],
            )
        };
        OpenPane {
            program,
            args,
            cwd: std::env::current_dir().expect("cwd"),
            env: Vec::new(),
            rows: 24,
            cols: 80,
            session_id: None,
        }
    }

    /// Drains a client's queue until `want` is seen in its output, or time runs
    /// out. Returns everything seen, so a failure can show what did arrive.
    fn wait_for(inbox: &Receiver<Message>, want: &str, deadline: Duration) -> String {
        let mut seen = String::new();
        let started = Instant::now();
        while started.elapsed() < deadline {
            if let Ok(Message::Output { data, .. }) = inbox.recv_timeout(Duration::from_millis(100))
            {
                seen.push_str(&String::from_utf8_lossy(&data));
                if seen.contains(want) {
                    break;
                }
            }
        }
        seen
    }

    #[test]
    fn a_pane_opens_speaks_and_is_seen_by_an_attached_client() {
        let dir = tempfile::tempdir().expect("tempdir");
        let panes = Panes::start(dir.path().to_path_buf()).expect("start");
        let (client, inbox) = panes.connect();

        let info = panes.open(&echoing("daemon-hears-this")).expect("open");
        panes.attach(client, info.pane, 0).expect("attach");

        let seen = wait_for(&inbox, "daemon-hears-this", Duration::from_secs(20));
        assert!(seen.contains("daemon-hears-this"), "{seen:?}");
        panes.close_all("test over");
    }

    /// Reattach is the reason the daemon exists. A client that arrives *after*
    /// everything has already happened must still see the screen.
    #[test]
    fn a_client_that_arrives_late_is_given_the_whole_history() {
        let dir = tempfile::tempdir().expect("tempdir");
        let panes = Panes::start(dir.path().to_path_buf()).expect("start");

        let info = panes
            .open(&echoing("said-before-anyone-looked"))
            .expect("open");
        // Let the child run and exit with nobody attached at all.
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(20) {
            if panes
                .list()
                .iter()
                .any(|p| p.pane == info.pane && !p.alive())
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let (client, inbox) = panes.connect();
        panes.attach(client, info.pane, 0).expect("attach");
        let seen = wait_for(&inbox, "said-before-anyone-looked", Duration::from_secs(10));
        assert!(
            seen.contains("said-before-anyone-looked"),
            "the byte log is what makes reattach work: {seen:?}"
        );
        panes.close_all("test over");
    }

    /// A client that was following and dropped comes back with an offset, and
    /// gets exactly the gap — no repeat, no hole.
    #[test]
    fn catching_up_from_an_offset_sends_only_the_gap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let panes = Panes::start(dir.path().to_path_buf()).expect("start");
        let info = panes.open(&echoing("first-half")).expect("open");

        let (early, early_inbox) = panes.connect();
        panes.attach(early, info.pane, 0).expect("attach");
        let seen = wait_for(&early_inbox, "first-half", Duration::from_secs(20));
        assert!(seen.contains("first-half"), "{seen:?}");
        let consumed = seen.len() as u64;

        let (late, late_inbox) = panes.connect();
        panes.attach(late, info.pane, consumed).expect("attach");
        // Whatever arrives must be the *rest*, never a repeat of what was seen.
        let mut tail = String::new();
        while let Ok(Message::Output { offset, data, .. }) =
            late_inbox.recv_timeout(Duration::from_millis(500))
        {
            assert!(offset >= consumed, "no rewind: {offset} < {consumed}");
            tail.push_str(&String::from_utf8_lossy(&data));
        }
        assert!(
            !tail.contains("first-half"),
            "the gap was replayed twice: {tail:?}"
        );
        panes.close_all("test over");
    }

    /// A pane whose child exited keeps its record, its history and its code —
    /// a pane that vanishes takes the error message with it.
    #[test]
    fn an_exited_pane_keeps_its_screen_and_its_exit_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        let panes = Panes::start(dir.path().to_path_buf()).expect("start");
        let (client, inbox) = panes.connect();
        let (program, args) = if cfg!(windows) {
            ("cmd".to_owned(), vec!["/c".to_owned(), "exit 3".to_owned()])
        } else {
            ("sh".to_owned(), vec!["-c".to_owned(), "exit 3".to_owned()])
        };
        let info = panes
            .open(&OpenPane {
                program,
                args,
                cwd: std::env::current_dir().expect("cwd"),
                env: Vec::new(),
                rows: 24,
                cols: 80,
                session_id: None,
            })
            .expect("open");
        panes.attach(client, info.pane, 0).expect("attach");

        let started = Instant::now();
        let mut code = None;
        while started.elapsed() < Duration::from_secs(20) {
            if let Ok(Message::Exit { pane, code: got }) =
                inbox.recv_timeout(Duration::from_millis(200))
            {
                assert_eq!(pane, info.pane);
                code = got;
                break;
            }
        }
        assert_eq!(code, Some(3), "the exit code has to survive");
        let still_there = panes.list();
        assert_eq!(still_there.len(), 1, "the pane stays after its child goes");
        assert!(!still_there[0].alive());
        panes.close_all("test over");
    }

    #[test]
    fn closing_a_pane_removes_it_and_tells_everyone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let panes = Panes::start(dir.path().to_path_buf()).expect("start");
        let (_client, inbox) = panes.connect();
        let info = panes.open(&echoing("gone-soon")).expect("open");
        assert_eq!(panes.count(), 1);
        panes.close(info.pane).expect("close");
        assert_eq!(panes.count(), 0);
        assert!(panes.close(info.pane).is_err(), "closing twice says so");

        let mut saw_closed = false;
        while let Ok(message) = inbox.try_recv() {
            if matches!(message, Message::Closed { pane } if pane == info.pane) {
                saw_closed = true;
            }
        }
        assert!(saw_closed, "every client hears about a closed pane");
    }

    #[test]
    fn a_bad_program_leaves_nothing_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let panes = Panes::start(dir.path().to_path_buf()).expect("start");
        let mut request = echoing("never runs");
        request.program = "polis-not-a-real-agent".to_owned();
        let error = panes.open(&request).expect_err("must fail");
        assert!(error.contains("polis-not-a-real-agent"), "{error}");
        assert_eq!(panes.count(), 0, "no half-open pane is left over");
    }

    /// A disconnected client must not keep a pane alive, and must not stop one.
    #[test]
    fn disconnecting_a_client_does_not_touch_the_panes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let panes = Panes::start(dir.path().to_path_buf()).expect("start");
        let (client, _inbox) = panes.connect();
        let info = panes.open(&echoing("keeps-running")).expect("open");
        panes.attach(client, info.pane, 0).expect("attach");
        panes.disconnect(client);
        assert_eq!(panes.client_count(), 0);
        assert_eq!(panes.count(), 1, "the agent outlives the window");
        panes.close_all("test over");
    }
}
