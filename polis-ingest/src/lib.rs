//! `polis-ingest` — the four data channels (PRD §4, §14).
//!
//! > Four channels, each chosen for its cost profile. **Do not use hooks for the
//! > firehose.**
//!
//! | Module | Channel | Cost profile |
//! |---|---|---|
//! | [`otel`] | A — OpenTelemetry | in-process, batched, zero spawn cost; carries the bulk traffic |
//! | [`hook`] | B — Hooks | one process spawn per event; rare and latency-critical only |
//! | [`fs`] | C — Filesystem watch | out of band, zero agent impact, no attribution |
//! | [`transcript`] | D — JSONL tailing | reconciliation, cold start, replay |
//!
//! All four normalise into [`polis_events::Event`] and push into [`bus`].
//!
//! # Every channel is independently optional
//!
//! A Polis that dies because a stale collector holds port 4317 is strictly worse
//! than a Polis with no OTel. Each channel reports its own failure as
//! [`ControlEvent::ChannelDegraded`](polis_events::ControlEvent::ChannelDegraded)
//! and the rest keep running (ADR-0011).
//!
//! # Threading
//!
//! There is no `#[tokio::main]` anywhere in Polis: winit owns the main thread
//! (PRD §13). [`otel`] builds a private two-worker multi-threaded runtime on a
//! named background thread, and the socket is bound **synchronously on the
//! calling thread** before being handed to tokio, so `AddrInUse` surfaces as a
//! matchable `io::Error` rather than a panic inside a background task nobody
//! joins.

pub mod bus;
pub mod env;
pub mod fs;
pub mod hook;
pub mod otel;
pub mod transcript;

use std::path::PathBuf;

use polis_events::{Channel, PathMapper};

pub use bus::{EventSink, EventSource};

/// How to start the four channels (PRD §4).
#[derive(Debug, Clone)]
pub struct IngestConfig {
    /// Repository root; seeds the [`PathMapper`] every channel normalises through.
    pub repo_root: PathBuf,
    /// OTLP/gRPC bind address. **Always `127.0.0.1:4317`**: agents are
    /// configured to talk to that port, so falling back to another one would
    /// silently produce an empty city.
    pub otlp_addr: std::net::SocketAddr,
    /// Hook datagram bind address; `127.0.0.1:45177` by default.
    pub hook_addr: std::net::SocketAddrV4,
    /// `~/.claude/projects`, or an override for tests and replay.
    pub claude_projects_dir: PathBuf,
    /// Channels to start. A milestone that does not need a channel leaves it off
    /// rather than starting it and ignoring it.
    pub channels: ChannelSet,
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

/// A running ingest stack. Dropping it shuts every channel down.
#[derive(Debug)]
pub struct Ingest {
    _private: (),
}

impl Ingest {
    /// Starts every enabled channel and returns the consumer end of the bus.
    ///
    /// Never fails because a channel could not start: a channel that cannot bind
    /// emits [`ControlEvent::ChannelDegraded`] and the others carry on.
    ///
    /// [`ControlEvent::ChannelDegraded`]: polis_events::ControlEvent::ChannelDegraded
    pub fn start(config: IngestConfig, mapper: PathMapper) -> (Self, EventSource) {
        let _ = (config, mapper);
        todo!("PRD §4 — start the enabled channels, each independently fallible")
    }

    /// Requests shutdown and waits for every channel thread to stop.
    pub fn shutdown(self) {
        todo!("PRD §4.5 — flush the bus, then join")
    }
}
