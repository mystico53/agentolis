//! `polis-ingest` — the four data channels (PRD §4, §14).
//!
//! > Four channels, each chosen for its cost profile. **Do not use hooks for the
//! > firehose.**
//!
//! | Module | Channel | Cost profile |
//! |---|---|---|
//! | [`otlp`] | A — OpenTelemetry | in-process, batched, zero spawn cost; carries the bulk traffic |
//! | [`hook_listener`] | B — Hooks | one process spawn per event; rare and latency-critical only |
//! | [`fswatch`] | C — Filesystem watch | out of band, zero agent impact, no attribution |
//! | [`transcript`] | D — JSONL tailing | reconciliation, cold start, replay |
//!
//! Two modules serve all four: [`normalize`] turns a decoded record of any
//! channel into a [`polis_events::Event`], and [`bus`] carries the result to the
//! world thread. [`mod@env`] is not a channel — it writes the configuration that
//! makes channels A and B exist at all.
//!
//! # Every channel is independently optional
//!
//! A Polis that dies because a stale collector holds port 4317 is strictly worse
//! than a Polis with no OTel. Each channel reports its own failure as
//! [`ControlEvent::ChannelDegraded`](polis_events::ControlEvent::ChannelDegraded)
//! and the rest keep running (ADR-0011). The one exception is the hook
//! listener's `AddrInUse`, which means a second Polis is already running and is
//! fatal by design — see [`hook_listener`].
//!
//! # Threading
//!
//! There is no `#[tokio::main]` anywhere in Polis: winit owns the main thread
//! (PRD §13). [`otlp`] builds a private two-worker multi-threaded runtime on a
//! named background thread, and the socket is bound **synchronously on the
//! calling thread** before being handed to tokio, so `AddrInUse` surfaces as a
//! matchable `io::Error` rather than a panic inside a background task nobody
//! joins. The other three channels are plain OS threads.
//!
//! # Ownership of the shared surface
//!
//! The types in this file, plus [`bus::EventSink`], [`bus::EventSource`],
//! [`bus::BusStats`] and [`bus::Push`], are the crate's **cross-module
//! contract**: four channel modules are written against them concurrently.
//! Their names and signatures are fixed. Change what is *behind* them freely;
//! changing the shapes themselves breaks four implementations at once and needs
//! a contract change, not an edit.

pub mod bus;
pub mod env;
pub mod fswatch;
pub mod hook_listener;
pub mod normalize;
pub mod otlp;
pub mod transcript;

use std::fmt;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};

use polis_events::{Channel, ControlEvent, PathMapper, DEFAULT_HOOK_PORT, EVENT_BUS_CAPACITY};

pub use bus::{BusStats, BusTotals, EventSink, EventSource, Push};

/// The default OTLP/gRPC endpoint (PRD §4.1).
///
/// **Always 4317.** Agents are configured to talk to that port, so binding
/// anywhere else produces an empty city that looks healthy (ADR-0011).
pub const DEFAULT_OTLP_ADDR: &str = "127.0.0.1:4317";

// ---------------------------------------------------------------------------
// What a channel is
// ---------------------------------------------------------------------------

/// The lifecycle contract every ingest channel implements (PRD §4).
///
/// Object-safe on purpose: [`Ingest`] holds `Vec<Box<dyn IngestSource>>` so a
/// milestone that runs two channels and a milestone that runs four are the same
/// code path, and so `polis doctor` can report health without knowing which
/// channels exist.
///
/// A source's own `start` function is *not* on this trait: each takes different
/// arguments (a `SocketAddr`, a set of roots, a projects directory) and each has
/// a different error type. The trait begins once a channel is running.
pub trait IngestSource: Send + fmt::Debug {
    /// Which channel this is. One [`Channel`] value per implementor;
    /// [`Channel::Control`] is not a source.
    fn channel(&self) -> Channel;

    /// Current health, for the status bar and `polis doctor`.
    ///
    /// Cheap and non-blocking: this is polled from the UI thread.
    fn health(&self) -> SourceHealth;

    /// Requests a stop and joins the channel's threads.
    ///
    /// Takes `Box<Self>` rather than `self` to stay object-safe. Must be
    /// idempotent in effect and must not panic: shutdown runs while the process
    /// is already on its way out.
    fn shutdown(self: Box<Self>);
}

/// What a channel is currently doing (PRD §4, ADR-0011).
///
/// `#[non_exhaustive]` for the same reason
/// [`polis_events::ControlEvent`] is: the list of ways a channel can be unwell
/// grows with field experience, and the status bar lacking a rendering for a new
/// one is cosmetic where a compile break is not.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SourceHealth {
    /// Running normally.
    Running,
    /// Running, but not delivering everything it should. Carries the operator-
    /// readable reason that goes into
    /// [`ControlEvent::ChannelDegraded`](polis_events::ControlEvent::ChannelDegraded).
    ///
    /// The canonical cases: port 4317 held by a stale collector, and the beta
    /// traces channel producing no spans — in which case every tool call is
    /// attributed to the main agent and the session is marked degraded rather
    /// than guessed at (ADR-0006).
    Degraded {
        /// Why, in words an operator can act on.
        reason: String,
    },
    /// Never started, or stopped and not restarting.
    Stopped {
        /// Why, in words an operator can act on.
        reason: String,
    },
}

impl SourceHealth {
    /// True while the channel is delivering everything it is supposed to.
    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Running)
    }

    /// The operator-readable reason, when there is one.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Running => None,
            Self::Degraded { reason } | Self::Stopped { reason } => Some(reason),
        }
    }
}

impl fmt::Display for SourceHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => f.write_str("running"),
            Self::Degraded { reason } => write!(f, "degraded: {reason}"),
            Self::Stopped { reason } => write!(f, "stopped: {reason}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// How to start the four channels (PRD §4).
#[derive(Debug, Clone)]
pub struct IngestConfig {
    /// Repository root; seeds the [`PathMapper`] every channel normalises through.
    pub repo_root: PathBuf,
    /// OTLP/gRPC bind address. **Always `127.0.0.1:4317`** — see
    /// [`DEFAULT_OTLP_ADDR`].
    pub otlp_addr: std::net::SocketAddr,
    /// Hook datagram bind address; `127.0.0.1:45177` by default
    /// ([`polis_events::DEFAULT_HOOK_PORT`], ADR-0026).
    pub hook_addr: std::net::SocketAddrV4,
    /// `~/.claude/projects`, or an override for tests and replay.
    pub claude_projects_dir: PathBuf,
    /// Channels to start. A milestone that does not need a channel leaves it off
    /// rather than starting it and ignoring it.
    pub channels: ChannelSet,
    /// Bus capacity. [`polis_events::EVENT_BUS_CAPACITY`] outside tests.
    pub bus_capacity: usize,
}

impl IngestConfig {
    /// Every default, for a repository at `repo_root`.
    ///
    /// The OTLP address is **always** [`DEFAULT_OTLP_ADDR`]; the hook address is
    /// always [`polis_events::DEFAULT_HOOK_PORT`] on loopback (ADR-0026). Both
    /// are fields rather than constants only so tests can take an ephemeral
    /// port — production never varies them, because a different port produces an
    /// empty city that looks healthy.
    ///
    /// `claude_projects_dir` falls back to the repository root when there is no
    /// home directory to derive one from; Channel D then finds nothing, which is
    /// a degraded channel rather than a startup failure.
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        let repo_root = repo_root.into();
        let claude_projects_dir =
            default_claude_projects_dir().unwrap_or_else(|| repo_root.join(".claude/projects"));
        Self {
            repo_root,
            otlp_addr: default_otlp_addr(),
            hook_addr: SocketAddrV4::new(Ipv4Addr::LOCALHOST, DEFAULT_HOOK_PORT),
            claude_projects_dir,
            channels: ChannelSet::ALL,
            bus_capacity: EVENT_BUS_CAPACITY,
        }
    }

    /// This configuration with a different channel set.
    #[must_use]
    pub fn with_channels(mut self, channels: ChannelSet) -> Self {
        self.channels = channels;
        self
    }
}

/// [`DEFAULT_OTLP_ADDR`], parsed.
///
/// Infallible in practice — the constant is a literal socket address — and the
/// fallback repeats it numerically rather than panicking, because a `todo`-free
/// startup path must not have an `unwrap` in it.
pub fn default_otlp_addr() -> SocketAddr {
    DEFAULT_OTLP_ADDR
        .parse()
        .unwrap_or(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4317)))
}

/// Which channels to run, as a set rather than four independent flags.
///
/// A set, because "which channels are on" is one decision with four members and
/// the members are already enumerated by [`Channel`]. Four `bool` fields would
/// let this type and [`Channel`] drift apart, and PRD §4 refers to the channels
/// by letter throughout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelSet(u8);

impl ChannelSet {
    /// Every Claude Code channel.
    pub const ALL: Self = Self(0b1111);
    /// No channels — what `polis snapshot` wants: a city and no agents.
    pub const NONE: Self = Self(0);

    /// Builds a set from channel names, as they appear in the config file.
    pub fn from_channels(channels: &[Channel]) -> Self {
        channels.iter().fold(Self::NONE, |set, c| set.with(*c))
    }

    /// Whether a channel is enabled.
    pub fn contains(self, channel: Channel) -> bool {
        let bit = Self::bit(channel);
        bit != 0 && self.0 & bit != 0
    }

    /// This set plus `channel`.
    #[must_use]
    pub fn with(self, channel: Channel) -> Self {
        Self(self.0 | Self::bit(channel))
    }

    /// This set minus `channel`.
    #[must_use]
    pub fn without(self, channel: Channel) -> Self {
        Self(self.0 & !Self::bit(channel))
    }

    /// True when no Claude Code channel is enabled.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    fn bit(channel: Channel) -> u8 {
        match channel {
            Channel::Otel => 0b0001,
            Channel::Hook => 0b0010,
            Channel::Fs => 0b0100,
            Channel::Transcript => 0b1000,
            // Polis's own health signals are not an ingest channel and cannot be
            // switched off.
            Channel::Control => 0,
        }
    }
}

impl Default for ChannelSet {
    fn default() -> Self {
        Self::ALL
    }
}

// ---------------------------------------------------------------------------
// The running stack
// ---------------------------------------------------------------------------

/// A running ingest stack. Dropping it shuts every channel down.
#[derive(Debug)]
pub struct Ingest {
    /// Channels that started, in the order they were started.
    sources: Vec<Box<dyn IngestSource>>,
    /// Channels that were enabled and did **not** start, with the reason. Kept
    /// so [`Ingest::health`] reports a channel that is off, rather than
    /// reporting nothing and looking healthy (ADR-0011).
    failed: Vec<(Channel, SourceHealth)>,
    /// A sink of Polis's own, held so the bus stays open even when every
    /// channel is off. Dropping it is what makes [`EventSource::recv`] return
    /// `None`, so it is dropped exactly once, in [`Ingest::shutdown`].
    sink: Option<EventSink>,
    /// Set when the hook port was already bound — i.e. another Polis is already
    /// running. See [`Ingest::hook_conflict`].
    hook_conflict: Option<String>,
    /// Where the hook endpoint file was published, when it was.
    endpoint: Option<PathBuf>,
}

impl Ingest {
    /// Starts every enabled channel and returns the consumer end of the bus.
    ///
    /// Never fails because a channel could not start: a channel that cannot bind
    /// emits [`ControlEvent::ChannelDegraded`] and the others carry on
    /// (ADR-0011). A developer with a stale collector on 4317 still gets hooks,
    /// filesystem events and transcripts.
    ///
    /// The single exception is the hook listener's `AddrInUse` — two daemons
    /// would each see roughly half the events and neither would say so. That is
    /// still not allowed to abort the other three channels here, because
    /// `Ingest::start` has no error to return and silently starting nothing
    /// would be worse; it is reported by [`Ingest::hook_conflict`] and the
    /// binary turns it into a clear exit (ADR-0026).
    ///
    /// [`ControlEvent::ChannelDegraded`]: polis_events::ControlEvent::ChannelDegraded
    pub fn start(config: IngestConfig, mapper: PathMapper) -> (Self, EventSource) {
        let IngestConfig {
            repo_root,
            otlp_addr,
            hook_addr,
            claude_projects_dir,
            channels,
            bus_capacity,
        } = config;
        let (sink, source) = bus::channel(bus_capacity);
        let mut ingest = Self {
            sources: Vec::new(),
            failed: Vec::new(),
            sink: Some(sink.clone()),
            hook_conflict: None,
            endpoint: None,
        };

        // Order is Channel order, and it is deliberate: the two channels that
        // bind a port go first, so a conflict is known before any thread that
        // could produce events has been spawned.
        if channels.contains(Channel::Otel) {
            ingest.start_otlp(otlp_addr, &sink, mapper.clone());
        }
        if channels.contains(Channel::Hook) {
            ingest.start_hooks(hook_addr, &sink, mapper.clone());
        }
        if channels.contains(Channel::Fs) {
            ingest.start_fs(&repo_root, &sink, mapper.clone());
        }
        if channels.contains(Channel::Transcript) {
            ingest.start_transcripts(&claude_projects_dir, &sink, mapper);
        }
        (ingest, source)
    }

    fn start_otlp(&mut self, addr: SocketAddr, sink: &EventSink, mapper: PathMapper) {
        match otlp::OtlpReceiver::start(addr, sink.clone(), mapper) {
            Ok(receiver) => self.sources.push(Box::new(receiver)),
            Err(error) => {
                // NEVER pick a different port: agents are configured to talk to
                // 4317, so binding elsewhere yields an empty city that looks
                // healthy (ADR-0011). Match the kind, never the message — the
                // Windows string is localised.
                let reason = if error.kind() == ErrorKind::AddrInUse {
                    format!(
                        "{addr} is already bound (a stale collector, or a second Polis): running \
                         without Channel A. Polis never falls back to another port, because \
                         agents are configured to export to that one"
                    )
                } else {
                    format!("cannot bind {addr}: {error}")
                };
                self.degrade(sink, Channel::Otel, reason);
            }
        }
    }

    fn start_hooks(&mut self, addr: SocketAddrV4, sink: &EventSink, mapper: PathMapper) {
        match hook_listener::HookListener::start(addr, sink.clone(), mapper) {
            Ok(listener) => {
                // Publication comes AFTER the bind, never before: the bind *is*
                // the singleton lock, so a losing daemon returns `Err` above and
                // never touches the winner's file (ADR-0026).
                match listener.publish_endpoint() {
                    Ok(path) => self.endpoint = Some(path),
                    Err(error) => {
                        // Not degraded: hooks still arrive on the compiled-in
                        // 45177, which is exactly where this listener is bound.
                        tracing::warn!(%error, "could not publish the hook endpoint file");
                    }
                }
                self.sources.push(Box::new(listener));
            }
            Err(error) => {
                let reason = error.to_string();
                if error.kind() == ErrorKind::AddrInUse {
                    self.hook_conflict = Some(reason.clone());
                }
                self.degrade(sink, Channel::Hook, reason);
            }
        }
    }

    fn start_fs(&mut self, repo_root: &Path, sink: &EventSink, mapper: PathMapper) {
        // Every registered worktree, not only the primary: PRD §7.6 makes
        // worktrees first-class and the whole point is seeing two agents edit
        // the same logical file on two branches.
        let roots: Vec<(polis_events::WorktreeId, PathBuf)> = mapper
            .roots()
            .map(|(id, root)| (id, PathBuf::from(root)))
            .collect();
        let roots: Vec<(polis_events::WorktreeId, &Path)> = if roots.is_empty() {
            vec![(polis_events::WorktreeId::PRIMARY, repo_root)]
        } else {
            roots.iter().map(|(id, p)| (*id, p.as_path())).collect()
        };
        match fswatch::FsWatcher::start(&roots, sink.clone(), mapper) {
            Ok(watcher) => self.sources.push(Box::new(watcher)),
            Err(error) => self.degrade(
                sink,
                Channel::Fs,
                format!(
                    "cannot watch {}: {error}; running without Channel C",
                    repo_root.display()
                ),
            ),
        }
    }

    fn start_transcripts(&mut self, projects_dir: &Path, sink: &EventSink, mapper: PathMapper) {
        match transcript::TranscriptTailer::start_discovering(projects_dir, sink.clone(), mapper) {
            Ok(tailer) => self.sources.push(Box::new(tailer)),
            Err(error) => self.degrade(
                sink,
                Channel::Transcript,
                format!(
                    "cannot read {}: {error}; running without Channel D (no reconciliation, no \
                     cold-start rebuild)",
                    projects_dir.display()
                ),
            ),
        }
    }

    /// Records a channel that did not start and announces it on the bus.
    fn degrade(&mut self, sink: &EventSink, channel: Channel, reason: String) {
        tracing::warn!(%channel, %reason, "ingest channel degraded");
        sink.push_control(ControlEvent::ChannelDegraded {
            channel,
            reason: reason.clone(),
        });
        self.failed
            .push((channel, SourceHealth::Stopped { reason }));
    }

    /// Health of every enabled channel, in [`Channel`] order.
    ///
    /// Includes channels that never started: a channel that is simply absent
    /// from this list would be indistinguishable from a healthy one.
    pub fn health(&self) -> Vec<(Channel, SourceHealth)> {
        let mut out: Vec<(Channel, SourceHealth)> = self
            .sources
            .iter()
            .map(|s| (s.channel(), s.health()))
            .chain(self.failed.iter().cloned())
            .collect();
        out.sort_by_key(|(channel, _)| channel_order(*channel));
        out
    }

    /// The operator message for "another Polis already holds the hook port".
    ///
    /// `Some` means a second daemon is running: two of them would each see
    /// roughly half the hook traffic and neither would say so, which is why
    /// ADR-0026 makes this a clear exit rather than a degraded channel. The
    /// other three channels are already running by the time this is readable,
    /// so the caller decides — a UI can carry on without hooks, a daemon
    /// should not.
    pub fn hook_conflict(&self) -> Option<&str> {
        self.hook_conflict.as_deref()
    }

    /// Where the hook endpoint file was published, when it was.
    pub fn endpoint_file(&self) -> Option<&Path> {
        self.endpoint.as_deref()
    }

    /// Bus counters: what was dropped, per channel (PRD §4.5).
    pub fn stats(&self) -> BusStats {
        self.sink.as_ref().map(EventSink::stats).unwrap_or_default()
    }

    /// Bus counters: what was received, per channel.
    pub fn totals(&self) -> BusTotals {
        self.sink
            .as_ref()
            .map(EventSink::totals)
            .unwrap_or_default()
    }

    /// Requests shutdown and waits for every channel thread to stop.
    ///
    /// Announces [`ControlEvent::Shutdown`](polis_events::ControlEvent::Shutdown)
    /// first, so a consumer draining the bus learns why it is about to end, then
    /// drops Polis's own sink — which is what makes
    /// [`EventSource::recv`] return `None` once the channel threads have
    /// released theirs.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(sink) = self.sink.take() {
            sink.push_control(ControlEvent::Shutdown);
            drop(sink);
        }
        for source in std::mem::take(&mut self.sources) {
            source.shutdown();
        }
    }
}

impl Drop for Ingest {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Sort key putting channels in PRD §4's A, B, C, D order.
fn channel_order(channel: Channel) -> u8 {
    match channel {
        Channel::Otel => 0,
        Channel::Hook => 1,
        Channel::Fs => 2,
        Channel::Transcript => 3,
        Channel::Control => 4,
    }
}

/// `~/.claude/projects`, where Channel D looks for transcripts.
///
/// `USERPROFILE` first on Windows, `HOME` elsewhere, and each as the other's
/// fallback: a Git Bash shell sets `HOME` on Windows too, and an operator who
/// has moved their profile should not silently get an empty Channel D.
pub fn default_claude_projects_dir() -> Option<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .or_else(|| std::env::var_os(if cfg!(windows) { "HOME" } else { "USERPROFILE" }))?;
    Some(PathBuf::from(home).join(".claude").join("projects"))
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, UdpSocket};

    use polis_events::{ControlEvent, Payload};

    use super::*;

    /// A stack rooted in two temporary directories, on ephemeral ports.
    ///
    /// Never the real 4317 or 45177: a test that binds those fights whatever the
    /// developer is running, and — worse — a hook listener on 45177 publishes the
    /// endpoint file every real agent on the machine reads.
    fn scratch_config(dir: &Path) -> (IngestConfig, PathMapper) {
        let repo = dir.join("repo");
        let projects = dir.join("projects");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        let mapper = PathMapper::new(&repo).expect("a temp dir is mappable");
        let mut config = IngestConfig::new(&repo);
        config.claude_projects_dir = projects;
        config.otlp_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
        config.hook_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        config.bus_capacity = 1_024;
        (config, mapper)
    }

    fn health_of(health: &[(Channel, SourceHealth)], channel: Channel) -> &SourceHealth {
        &health
            .iter()
            .find(|(c, _)| *c == channel)
            .unwrap_or_else(|| panic!("{channel} is missing from health()"))
            .1
    }

    #[test]
    fn a_channel_that_cannot_bind_degrades_and_never_takes_the_others_down() {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, mapper) = scratch_config(dir.path());
        // Somebody else's collector on the OTLP port. Polis must run without
        // Channel A and must NOT silently move to another port (ADR-0011).
        let squatter = TcpListener::bind("127.0.0.1:0").unwrap();
        config.otlp_addr = squatter.local_addr().unwrap();
        // Hooks off: a listener here would publish the endpoint file that every
        // real agent on this machine reads.
        config.channels = ChannelSet::ALL.without(Channel::Hook);

        let (ingest, source) = Ingest::start(config, mapper);
        let health = ingest.health();
        assert!(
            matches!(
                health_of(&health, Channel::Otel),
                SourceHealth::Stopped { .. }
            ),
            "a taken OTLP port is a stopped channel, not a dead process: {health:?}"
        );
        assert!(
            health_of(&health, Channel::Fs).is_healthy(),
            "Channel C must survive Channel A's failure: {health:?}"
        );
        assert!(
            health_of(&health, Channel::Transcript).is_healthy(),
            "Channel D must survive Channel A's failure: {health:?}"
        );

        // The degradation is announced, not merely recorded: a silently missing
        // channel is exactly the failure PRD §4.5's status bar exists to catch.
        let mut batch = Vec::new();
        source.drain(&mut batch);
        assert!(
            batch.iter().any(|e| matches!(
                &e.payload,
                Payload::Control(ControlEvent::ChannelDegraded {
                    channel: Channel::Otel,
                    ..
                })
            )),
            "no ChannelDegraded for the OTLP port: {batch:?}"
        );
        ingest.shutdown();
    }

    #[test]
    fn a_second_polis_is_reported_rather_than_silently_halving_the_hook_stream() {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, mapper) = scratch_config(dir.path());
        // Stand in for the first Polis. Because the bind fails, the losing
        // daemon never reaches `publish_endpoint` and cannot clobber the
        // winner's endpoint file (ADR-0026).
        let first = UdpSocket::bind("127.0.0.1:0").unwrap();
        let SocketAddr::V4(taken) = first.local_addr().unwrap() else {
            panic!("bound v4")
        };
        config.hook_addr = taken;
        config.channels = ChannelSet::from_channels(&[Channel::Hook, Channel::Transcript]);

        let (ingest, _source) = Ingest::start(config, mapper);
        let conflict = ingest
            .hook_conflict()
            .expect("AddrInUse on the hook port means another Polis is running");
        assert!(
            conflict.contains(&taken.to_string()),
            "the operator message must name the address: {conflict}"
        );
        assert!(
            ingest.endpoint_file().is_none(),
            "the loser publishes nothing"
        );
        // Still not fatal *here*: the caller decides. Channel D kept running.
        assert!(health_of(&ingest.health(), Channel::Transcript).is_healthy());
        ingest.shutdown();
    }

    #[test]
    fn shutdown_closes_the_bus_so_a_consumer_loop_ends() {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, mapper) = scratch_config(dir.path());
        config.channels = ChannelSet::NONE;
        let (ingest, source) = Ingest::start(config, mapper);
        // With no channels at all the bus is still open, because `Ingest` holds
        // a sink of its own — otherwise `recv` would return `None` immediately
        // and a `polis tail` with `--source fs` alone would exit at once.
        ingest.shutdown();
        let mut seen_shutdown = false;
        while let Some(event) = source.recv() {
            if matches!(event.payload, Payload::Control(ControlEvent::Shutdown)) {
                seen_shutdown = true;
            }
        }
        assert!(seen_shutdown, "shutdown is announced before the bus closes");
    }

    #[test]
    fn a_channel_set_is_a_set_of_the_four_claude_code_channels() {
        assert!(ChannelSet::ALL.contains(Channel::Otel));
        assert!(ChannelSet::ALL.contains(Channel::Transcript));
        assert!(ChannelSet::NONE.is_empty());
        // Control is Polis's own health signal, not an ingest channel, so it can
        // never be switched on or off.
        assert!(!ChannelSet::ALL.contains(Channel::Control));
        assert!(!ChannelSet::NONE.contains(Channel::Control));

        let only_hooks = ChannelSet::from_channels(&[Channel::Hook]);
        assert!(only_hooks.contains(Channel::Hook));
        assert!(!only_hooks.contains(Channel::Otel));
        assert_eq!(
            only_hooks.with(Channel::Otel).without(Channel::Otel),
            only_hooks
        );
    }

    #[test]
    fn health_reports_a_reason_whenever_it_is_not_running() {
        assert!(SourceHealth::Running.is_healthy());
        assert_eq!(SourceHealth::Running.reason(), None);

        let degraded = SourceHealth::Degraded {
            reason: "port 4317 held by another process".to_owned(),
        };
        assert!(!degraded.is_healthy());
        assert_eq!(degraded.reason(), Some("port 4317 held by another process"));
        assert!(degraded.to_string().starts_with("degraded: "));
    }
}
