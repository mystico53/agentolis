//! The bounded event bus (PRD §4.5).
//!
//! > All four channels normalize into one internal `Event` enum and push into a
//! > bounded `crossbeam` channel (capacity 65536) consumed by the world-state
//! > thread. On full, drop oldest and increment a dropped-events counter
//! > surfaced in the status bar. **Backpressure must never propagate to an
//! > agent.**
//!
//! # Drop-oldest is not what `crossbeam` does by default
//!
//! `try_send` on a full bounded channel drops the **newest** and returns
//! `Full(msg)`. Drop-oldest requires the sink to hold a `Receiver` clone and
//! call `try_recv()` to evict before retrying — sound because crossbeam channels
//! are MPMC. A "simplification" back to a plain `try_send` silently inverts the
//! policy, which is why it is stated here rather than left to the implementation
//! (ADR-0008).
//!
//! # The sender must never await, block, or return an error
//!
//! Measured, the OTLP handler's send path costs ~50 µs whether the queue is
//! empty or permanently full: 2 000 RPCs into a capacity-8 channel with no
//! consumer gave p99 99 µs, zero errors, and exact drop accounting. Returning an
//! error `Status` instead would make the OTel exporter retry, which is
//! backpressure with extra steps.

use polis_events::{Channel, Event};

/// The producing half, cloned into every channel thread.
#[derive(Debug, Clone)]
pub struct EventSink {
    _private: (),
}

impl EventSink {
    /// Pushes an event, evicting the oldest if the bus is full.
    ///
    /// Infallible by design and never blocks. Returns `true` when nothing had to
    /// be evicted.
    pub fn push(&self, event: Event) -> bool {
        // Taken by value because the bus stores it. Dropped for now.
        drop(event);
        todo!("PRD §4.5 — try_recv to evict, then try_send; never block, never error")
    }

    /// Events dropped since the last read, for the status-bar counter.
    pub fn take_dropped(&self, channel: Channel) -> u64 {
        let _ = channel;
        todo!("PRD §4.5 — per-channel dropped counter")
    }
}

/// The consuming half. Owned by the world-state thread, which is the single
/// writer of `World` (PRD §5).
#[derive(Debug)]
pub struct EventSource {
    _private: (),
}

impl EventSource {
    /// Blocks until an event is available.
    pub fn recv(&self) -> Option<Event> {
        todo!("PRD §5 — single-writer consume loop")
    }

    /// Drains everything currently queued without blocking, returning how many
    /// events were appended.
    ///
    /// This is the shape the world thread actually wants: **decouple event rate
    /// from frame rate** (PRD §5), so it applies a whole batch and publishes one
    /// snapshot rather than one snapshot per event.
    pub fn drain(&self, out: &mut Vec<Event>) -> usize {
        let _ = out;
        todo!("PRD §5 — batch drain, one snapshot per batch")
    }
}

/// Builds a sink/source pair, in the shape of [`std::sync::mpsc::channel`].
///
/// Callers pass [`polis_events::EVENT_BUS_CAPACITY`] unless they are a test.
pub fn channel(capacity: usize) -> (EventSink, EventSource) {
    let _ = capacity;
    todo!("PRD §4.5 — bounded crossbeam channel plus a Receiver clone for eviction")
}
