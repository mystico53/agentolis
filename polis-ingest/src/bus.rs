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
//!
//! There is exactly one blocking call in this module — [`EventSource::recv`] —
//! and it is on the **consumer** side, which is the world thread and not an
//! agent. Nothing on the producing side blocks, awaits, or fails:
//! [`EventSink::push`] returns [`Push`] and has no error case at all, so a
//! caller cannot accidentally grow one.
//!
//! # These four types are the crate's cross-module contract
//!
//! [`EventSink`], [`EventSource`], [`BusStats`] and [`Push`] are re-exported
//! from the crate root and are used concurrently by [`crate::otlp`],
//! [`crate::hook_listener`], [`crate::fswatch`] and [`crate::transcript`].
//! Their names and signatures are fixed; the machinery behind them is not.
//! [`BusTotals`] is additive — it exists precisely so that the arrival and
//! drift counters could be added without reshaping [`BusStats`], which four
//! implementations are already written against.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, TrySendError};
use polis_events::{Channel, ControlEvent, Event, Payload};

/// How many times [`EventSink::push`] will evict-and-retry before giving up and
/// dropping the event it was handed (ADR-0008).
///
/// Two, not "until it fits": another producer can win the slot an eviction just
/// freed, and an unbounded retry loop under contention is a spin — which is a
/// blocking send wearing a different hat. Failing over to dropping the *newest*
/// after two attempts keeps the worst case bounded and still loses exactly one
/// event, which the counters record either way.
const EVICT_RETRIES: usize = 2;

/// Number of [`Channel`] values, and the width of the counter arrays.
const SLOTS: usize = 5;

/// Counter index for a channel.
///
/// [`Channel`] is deliberately not `#[non_exhaustive]` (it enumerates Polis's
/// own channels), so this match is exhaustive and a fifth channel breaks the
/// build here rather than silently sharing a counter with `Control`.
const fn slot(channel: Channel) -> usize {
    match channel {
        Channel::Otel => 0,
        Channel::Hook => 1,
        Channel::Fs => 2,
        Channel::Transcript => 3,
        Channel::Control => 4,
    }
}

/// What happened to a pushed event (PRD §4.5).
///
/// An enum rather than a `bool` because "true" would have to mean "nothing was
/// evicted", and every reader of a boolean return gets that backwards once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    /// The event was queued and nothing was lost.
    Queued,
    /// The bus was full: the oldest event was evicted to make room. The
    /// dropped-events counter has been incremented for the evicted event's
    /// channel, and the status bar will show it.
    Evicted,
}

impl Push {
    /// True when the push cost nothing.
    pub fn is_clean(self) -> bool {
        self == Self::Queued
    }
}

/// Drop accounting, for the status bar (PRD §4.5) and for PRD §13.1's
/// "500 events/sec with zero drops" assertion.
///
/// Per channel, because the interesting question is never "did we drop" but
/// "which channel is overrunning" — a flood on Channel A means an agent fleet
/// grew, whereas a flood on Channel B means something is wrong.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BusStats {
    /// Events dropped from the OTel channel since the process started.
    pub dropped_otel: u64,
    /// Events dropped from the hook channel.
    pub dropped_hook: u64,
    /// Events dropped from the filesystem channel.
    pub dropped_fs: u64,
    /// Events dropped from the transcript channel.
    pub dropped_transcript: u64,
    /// Events dropped from Polis's own control channel. Non-zero here means the
    /// bus is so far behind that even the drop notifications are being dropped.
    pub dropped_control: u64,
    /// Events currently queued.
    pub queued: usize,
    /// The bus's capacity, so a caller can render `queued / capacity` without
    /// having to remember what it was configured with.
    pub capacity: usize,
}

impl BusStats {
    /// Total drops across every channel.
    pub fn dropped_total(&self) -> u64 {
        self.dropped_otel
            .saturating_add(self.dropped_hook)
            .saturating_add(self.dropped_fs)
            .saturating_add(self.dropped_transcript)
            .saturating_add(self.dropped_control)
    }

    /// Drops on one channel.
    pub fn dropped(&self, channel: Channel) -> u64 {
        match channel {
            Channel::Otel => self.dropped_otel,
            Channel::Hook => self.dropped_hook,
            Channel::Fs => self.dropped_fs,
            Channel::Transcript => self.dropped_transcript,
            Channel::Control => self.dropped_control,
        }
    }
}

/// Arrival accounting, for the status bar's throughput readout and PRD §17's
/// drift warning.
///
/// Separate from [`BusStats`] rather than folded into it: [`BusStats`] is the
/// frozen cross-module shape four channel modules construct and destructure,
/// and a struct with public fields cannot gain one without breaking every
/// literal. These counters answer a different question anyway — [`BusStats`] is
/// "what did we lose", this is "what arrived, and is the schema moving".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BusTotals {
    /// Events pushed from the OTel channel since the process started, including
    /// any later evicted.
    pub received_otel: u64,
    /// Events pushed from the hook channel.
    pub received_hook: u64,
    /// Events pushed from the filesystem channel.
    pub received_fs: u64,
    /// Events pushed from the transcript channel.
    pub received_transcript: u64,
    /// Events pushed on Polis's own control channel.
    pub received_control: u64,
    /// How many [`ControlEvent::SchemaDrift`] events have crossed the bus.
    ///
    /// Counted here rather than at four call sites because PRD §17 wants *a
    /// schema-drift warning in the status bar rather than a crash*, and a
    /// warning nobody increments is a crash with extra steps. Every channel
    /// already pushes its drift signal through [`EventSink::push_control`], so
    /// counting it in the bus is the one place that cannot be forgotten.
    pub schema_drift: u64,
}

impl BusTotals {
    /// Total arrivals across every channel.
    pub fn received_total(&self) -> u64 {
        self.received_otel
            .saturating_add(self.received_hook)
            .saturating_add(self.received_fs)
            .saturating_add(self.received_transcript)
            .saturating_add(self.received_control)
    }

    /// Arrivals on one channel.
    pub fn received(&self, channel: Channel) -> u64 {
        match channel {
            Channel::Otel => self.received_otel,
            Channel::Hook => self.received_hook,
            Channel::Fs => self.received_fs,
            Channel::Transcript => self.received_transcript,
            Channel::Control => self.received_control,
        }
    }
}

/// The counters both halves of the bus share.
#[derive(Debug, Default)]
struct Counters {
    received: [AtomicU64; SLOTS],
    dropped: [AtomicU64; SLOTS],
    schema_drift: AtomicU64,
}

impl Counters {
    fn record_arrival(&self, event: &Event) {
        self.received[slot(event.meta.channel)].fetch_add(1, Ordering::Relaxed);
        if matches!(
            event.payload,
            Payload::Control(ControlEvent::SchemaDrift { .. })
        ) {
            self.schema_drift.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_drop(&self, channel: Channel) {
        self.dropped[slot(channel)].fetch_add(1, Ordering::Relaxed);
    }
}

/// Reads one channel's counter out of a counter array.
fn load(counters: &[AtomicU64; SLOTS], channel: Channel) -> u64 {
    counters[slot(channel)].load(Ordering::Relaxed)
}

/// The state a sink and its source share: the counters, and the capacity the
/// bus was built with.
#[derive(Debug)]
struct Bus {
    counters: Counters,
    capacity: usize,
}

impl Bus {
    fn stats(&self, queued: usize) -> BusStats {
        let d = &self.counters.dropped;
        BusStats {
            dropped_otel: load(d, Channel::Otel),
            dropped_hook: load(d, Channel::Hook),
            dropped_fs: load(d, Channel::Fs),
            dropped_transcript: load(d, Channel::Transcript),
            dropped_control: load(d, Channel::Control),
            queued,
            capacity: self.capacity,
        }
    }

    fn totals(&self) -> BusTotals {
        let r = &self.counters.received;
        BusTotals {
            received_otel: load(r, Channel::Otel),
            received_hook: load(r, Channel::Hook),
            received_fs: load(r, Channel::Fs),
            received_transcript: load(r, Channel::Transcript),
            received_control: load(r, Channel::Control),
            schema_drift: self.counters.schema_drift.load(Ordering::Relaxed),
        }
    }
}

/// The producing half, cloned into every channel thread.
///
/// Cloning is cheap and the clones share one queue and one set of counters.
///
/// The `evict` field is the whole trick: it is a `Receiver` clone held by the
/// *producer*, used only to pop the oldest entry when the queue is full. It is
/// never used to consume normally — that is [`EventSource`]'s job, and PRD §5
/// has exactly one consumer.
#[derive(Debug, Clone)]
pub struct EventSink {
    tx: Sender<Event>,
    evict: Receiver<Event>,
    bus: Arc<Bus>,
}

impl EventSink {
    /// Pushes an event, evicting the oldest if the bus is full.
    ///
    /// Infallible by design and never blocks: it is called from the OTLP
    /// handler, from the hook receive loop and from the watcher thread, and any
    /// of those blocking is backpressure reaching an agent.
    pub fn push(&self, event: Event) -> Push {
        self.bus.counters.record_arrival(&event);
        self.send_evicting(event)
    }

    /// Pushes one of Polis's own health signals.
    ///
    /// Separate from [`EventSink::push`] because every channel needs it and
    /// building the envelope by hand at four call sites is how two of them end
    /// up stamping the wrong [`Channel`].
    pub fn push_control(&self, control: ControlEvent) -> Push {
        self.push(Event::control(control))
    }

    /// A snapshot of the drop counters and queue depth.
    ///
    /// Cheap and non-blocking: the UI polls this every frame.
    pub fn stats(&self) -> BusStats {
        self.bus.stats(self.tx.len())
    }

    /// A snapshot of the arrival and schema-drift counters.
    pub fn totals(&self) -> BusTotals {
        self.bus.totals()
    }

    /// `try_send`, and on `Full` pop the oldest and try again (ADR-0008).
    ///
    /// Every exit from this function is a return; there is no path that waits.
    fn send_evicting(&self, event: Event) -> Push {
        let mut pending = event;
        let mut evicted = false;
        for _ in 0..EVICT_RETRIES {
            match self.tx.try_send(pending) {
                Ok(()) => {
                    return if evicted { Push::Evicted } else { Push::Queued };
                }
                Err(TrySendError::Full(returned)) => {
                    pending = returned;
                    // Pop one — the oldest, because crossbeam is FIFO — and
                    // count it against the channel that produced it, not
                    // against the channel that happened to overflow the bus.
                    if let Ok(old) = self.evict.try_recv() {
                        self.bus.counters.record_drop(old.meta.channel);
                        evicted = true;
                    }
                }
                Err(TrySendError::Disconnected(returned)) => {
                    // Unreachable while this sink lives, because the sink holds
                    // `evict`, a receiver. Kept total anyway: a lost event is a
                    // counted event, on every path.
                    self.bus.counters.record_drop(returned.meta.channel);
                    return Push::Evicted;
                }
            }
        }
        // Still full after the retry budget: another producer took the slot.
        // Drop the newest rather than spin, and say so.
        self.bus.counters.record_drop(pending.meta.channel);
        Push::Evicted
    }
}

/// The consuming half. Owned by the world-state thread, which is the single
/// writer of `World` (PRD §5).
///
/// Deliberately not `Clone`: crossbeam would happily let two consumers race, and
/// PRD §5's single-writer rule is the thing that makes the `arc-swap` snapshot
/// sound.
#[derive(Debug)]
pub struct EventSource {
    rx: Receiver<Event>,
    bus: Arc<Bus>,
}

impl EventSource {
    /// Blocks until an event is available. `None` once every sink is dropped.
    pub fn recv(&self) -> Option<Event> {
        self.rx.recv().ok()
    }

    /// Drains everything currently queued without blocking, returning how many
    /// events were appended.
    ///
    /// This is the shape the world thread actually wants: **decouple event rate
    /// from frame rate** (PRD §5), so it applies a whole batch and publishes one
    /// snapshot rather than one snapshot per event.
    ///
    /// Bounded by the bus capacity, so a producer faster than the consumer
    /// cannot hold the world thread inside this call indefinitely — the next
    /// frame gets its turn and the leftovers arrive in the batch after it.
    pub fn drain(&self, out: &mut Vec<Event>) -> usize {
        let mut n = 0;
        while n < self.bus.capacity {
            match self.rx.try_recv() {
                Ok(event) => {
                    out.push(event);
                    n += 1;
                }
                Err(_) => break,
            }
        }
        n
    }

    /// Blocks until at least one event is available, then drains the rest.
    ///
    /// The idle path: PRD §13.1 budgets under 2% of one core with no agent
    /// activity, which a `drain`-in-a-spin-loop cannot meet.
    pub fn recv_batch(&self, out: &mut Vec<Event>) -> usize {
        match self.rx.recv() {
            Ok(event) => {
                out.push(event);
                1 + self.drain(out)
            }
            Err(_) => 0,
        }
    }

    /// The same snapshot [`EventSink::stats`] returns.
    pub fn stats(&self) -> BusStats {
        self.bus.stats(self.rx.len())
    }

    /// The same snapshot [`EventSink::totals`] returns.
    pub fn totals(&self) -> BusTotals {
        self.bus.totals()
    }
}

/// Builds a sink/source pair, in the shape of [`std::sync::mpsc::channel`].
///
/// Callers pass [`polis_events::EVENT_BUS_CAPACITY`] unless they are a test.
///
/// A capacity of `0` is promoted to `1`: crossbeam reads zero as a *rendezvous*
/// channel, whose `try_send` fails unless a receiver is already parked, so a
/// zero-capacity bus would drop essentially everything while looking configured.
pub fn channel(capacity: usize) -> (EventSink, EventSource) {
    let capacity = capacity.max(1);
    let (tx, rx) = crossbeam_channel::bounded(capacity);
    let bus = Arc::new(Bus {
        counters: Counters::default(),
        capacity,
    });
    let sink = EventSink {
        tx,
        evict: rx.clone(),
        bus: Arc::clone(&bus),
    };
    (sink, EventSource { rx, bus })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::time::{Duration, Instant};

    use polis_events::{EventMeta, FsEvent, SessionId};

    use super::*;

    /// An event carrying an identity in `meta.sequence`, so a test can prove
    /// *which* events survived rather than only how many.
    fn numbered(channel: Channel, n: u64) -> Event {
        let mut meta = EventMeta::now(channel);
        meta.sequence = Some(n);
        Event::new(meta, Payload::Fs(FsEvent::RescanRequired))
    }

    /// Runs `body` on a worker thread and fails the test if it has not finished
    /// within `limit`.
    ///
    /// A blocking send has no observable difference from a slow one except that
    /// it never returns, so "no send ever blocks" is only testable as a
    /// deadline. The failure direction is exact: with no consumer and a
    /// capacity-8 bus, a `send` that blocks blocks forever.
    fn within<T: Send + 'static>(limit: Duration, body: impl FnOnce() -> T + Send + 'static) -> T {
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        thread::spawn(move || {
            let _ = done_tx.send(body());
        });
        done_rx
            .recv_timeout(limit)
            .expect("the bus blocked a producer; PRD §4.5 forbids that")
    }

    /// PRD §4.5's central claim, and the one crossbeam does not give for free:
    /// the bus drops the **oldest**, the counter is exact, and nothing waits.
    #[test]
    fn a_full_bus_drops_the_oldest_and_counts_it_exactly() {
        const CAP: usize = 8;
        const CAP64: u64 = 8;
        const PUSHES: u64 = 2_000;

        let (sink, source) = channel(CAP);
        let (stats, elapsed) = within(Duration::from_secs(30), move || {
            let start = Instant::now();
            for n in 0..PUSHES {
                // No consumer is running: after the eighth push every single
                // one of these is an eviction.
                let push = sink.push(numbered(Channel::Fs, n));
                assert_eq!(
                    push.is_clean(),
                    n < CAP64,
                    "push {n} reported the wrong disposition"
                );
            }
            (sink.stats(), start.elapsed())
        });

        assert_eq!(stats.queued, CAP);
        assert_eq!(stats.capacity, CAP);
        assert_eq!(
            stats.dropped_total(),
            PUSHES - CAP64,
            "drop accounting must be exact, not approximate"
        );
        assert_eq!(stats.dropped(Channel::Fs), stats.dropped_total());
        assert_eq!(stats.dropped(Channel::Otel), 0);

        // The survivors are the LAST eight, which is what distinguishes
        // drop-oldest from crossbeam's built-in drop-newest.
        let mut out = Vec::new();
        assert_eq!(source.drain(&mut out), CAP);
        let survivors: Vec<u64> = out.iter().filter_map(|e| e.meta.sequence).collect();
        assert_eq!(survivors, (PUSHES - CAP64..PUSHES).collect::<Vec<_>>());

        // Not a latency assertion — a sanity bound. 2 000 non-blocking pushes
        // cannot legitimately take seconds.
        assert!(
            elapsed < Duration::from_secs(5),
            "2 000 pushes took {elapsed:?}; the send path is not lock-free"
        );
    }

    /// The same property from the other side: a push on a permanently full bus
    /// costs about what a push on an empty one costs.
    #[test]
    fn no_single_push_stalls_on_a_permanently_full_bus() {
        let worst = within(Duration::from_secs(30), || {
            let (sink, _source) = channel(4);
            let mut worst = Duration::ZERO;
            for n in 0..500 {
                let at = Instant::now();
                sink.push(numbered(Channel::Otel, n));
                worst = worst.max(at.elapsed());
            }
            worst
        });
        assert!(
            worst < Duration::from_millis(250),
            "worst single push was {worst:?}; the handler must not wait on the bus"
        );
    }

    /// Several producers and one consumer, hammering. Asserts termination (a
    /// deadlock shows up as the deadline in `within`) and exact conservation:
    /// every pushed event was either delivered, dropped, or is still queued.
    #[test]
    fn many_producers_and_one_consumer_neither_deadlock_nor_lose_count() {
        const THREADS: u64 = 8;
        const EACH: u64 = 4_000;

        let (delivered, stats, totals) = within(Duration::from_secs(60), || {
            let (sink, source) = channel(64);
            let producing = Arc::new(AtomicBool::new(true));

            let consumer = {
                let producing = Arc::clone(&producing);
                thread::spawn(move || {
                    let mut batch = Vec::new();
                    let mut delivered = 0usize;
                    while producing.load(Ordering::Acquire) {
                        delivered += source.drain(&mut batch);
                        batch.clear();
                    }
                    // Producers are done; take whatever is left.
                    delivered += source.drain(&mut batch);
                    batch.clear();
                    (delivered, source)
                })
            };

            let producers: Vec<_> = (0..THREADS)
                .map(|t| {
                    let sink = sink.clone();
                    thread::spawn(move || {
                        let channel = match t % 3 {
                            0 => Channel::Otel,
                            1 => Channel::Hook,
                            _ => Channel::Fs,
                        };
                        for n in 0..EACH {
                            sink.push(numbered(channel, n));
                        }
                    })
                })
                .collect();
            for p in producers {
                p.join().expect("a producer panicked");
            }
            producing.store(false, Ordering::Release);
            let (delivered, source) = consumer.join().expect("the consumer panicked");

            let stats = source.stats();
            let totals = source.totals();
            drop(sink);
            (delivered, stats, totals)
        });

        let pushed = THREADS * EACH;
        let delivered = u64::try_from(delivered).expect("delivered count fits");
        let queued = u64::try_from(stats.queued).expect("queue depth fits");
        assert_eq!(totals.received_total(), pushed);
        assert_eq!(
            delivered + stats.dropped_total() + queued,
            pushed,
            "delivered {delivered} + dropped {} + queued {queued} != pushed {pushed}",
            stats.dropped_total()
        );
    }

    #[test]
    fn drop_and_arrival_counters_are_per_channel() {
        let (sink, _source) = channel(1);
        sink.push(numbered(Channel::Otel, 0));
        sink.push(numbered(Channel::Hook, 1));
        sink.push(numbered(Channel::Transcript, 2));

        let stats = sink.stats();
        // Capacity 1: the first two are evicted, in arrival order.
        assert_eq!(stats.dropped(Channel::Otel), 1);
        assert_eq!(stats.dropped(Channel::Hook), 1);
        assert_eq!(stats.dropped(Channel::Transcript), 0);
        assert_eq!(stats.dropped_total(), 2);

        let totals = sink.totals();
        assert_eq!(totals.received(Channel::Otel), 1);
        assert_eq!(totals.received(Channel::Transcript), 1);
        assert_eq!(totals.received(Channel::Fs), 0);
        assert_eq!(totals.received_total(), 3);
    }

    /// PRD §17: drift is a status-bar warning, so something has to count it.
    #[test]
    fn schema_drift_is_counted_as_it_crosses_the_bus() {
        let (sink, _source) = channel(16);
        assert_eq!(sink.totals().schema_drift, 0);

        sink.push_control(ControlEvent::SchemaDrift {
            channel: Channel::Otel,
            producer_version: Some("2.1.196".to_owned()),
            detail: "unknown event name `nucleation`".to_owned(),
        });
        sink.push_control(ControlEvent::Shutdown);
        sink.push_control(ControlEvent::EventsDropped {
            count: 3,
            channel: Channel::Fs,
        });

        let totals = sink.totals();
        assert_eq!(totals.schema_drift, 1, "only drift counts as drift");
        assert_eq!(totals.received(Channel::Control), 3);
    }

    #[test]
    fn recv_batch_blocks_for_the_first_event_then_takes_the_rest() {
        let (sink, source) = channel(16);
        let feeder = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            for n in 0..5 {
                sink.push(numbered(Channel::Hook, n));
            }
            sink
        });

        let mut out = Vec::new();
        let n = source.recv_batch(&mut out);
        assert!((1..=5).contains(&n), "recv_batch returned {n}");
        assert_eq!(out.len(), n);
        let sink = feeder.join().expect("feeder panicked");
        drop(sink);
    }

    #[test]
    fn the_source_ends_once_every_sink_is_dropped() {
        let (sink, source) = channel(4);
        let clone = sink.clone();
        sink.push(numbered(Channel::Fs, 0));
        drop(sink);
        drop(clone);

        assert!(source.recv().is_some());
        assert!(
            source.recv().is_none(),
            "a closed bus must end the world thread's loop, not park it"
        );
        let mut out = Vec::new();
        assert_eq!(source.recv_batch(&mut out), 0);
        assert!(out.is_empty());
    }

    /// A zero-capacity crossbeam channel is a rendezvous, whose `try_send`
    /// fails unless a receiver is parked. Left unguarded, `channel(0)` would
    /// drop every event while reporting a healthy-looking configuration.
    #[test]
    fn zero_capacity_is_promoted_rather_than_becoming_a_rendezvous() {
        let (sink, source) = channel(0);
        assert_eq!(sink.stats().capacity, 1);
        assert!(sink.push(numbered(Channel::Fs, 0)).is_clean());
        assert_eq!(sink.stats().queued, 1);
        assert!(source.recv().is_some());
    }

    /// The envelope survives the bus untouched — the world thread keys threads
    /// off `meta.session`, and a bus that rebuilt envelopes would break that.
    #[test]
    fn the_bus_carries_events_through_unmodified() {
        let (sink, source) = channel(4);
        let meta = EventMeta::now(Channel::Hook).with_session(SessionId::new("s-1"));
        sink.push(Event::new(meta, Payload::Fs(FsEvent::RescanRequired)));
        let got = source.recv().expect("one event");
        assert_eq!(got.meta.channel, Channel::Hook);
        assert_eq!(got.meta.thread.as_ref().unwrap().as_str(), "s-1");
    }
}
