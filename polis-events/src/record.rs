//! The on-disk recording format for `polis replay` (PRD §15 M2, ADR-0049).
//!
//! # Why the live path and the recorded path use different clocks
//!
//! [`crate::EventMeta::observed`] is a [`std::time::Instant`], and that is the right
//! choice for everything the running daemon does. It cannot step backwards, it
//! is immune to NTP corrections and to the operator changing the system clock
//! mid-session, and PRD §4.3's ±2 s attribution window and ADR-0014's
//! order-by-receipt rule both depend on exactly that property. Transcript
//! timestamps are *not* monotonic — 20% of files contain a backwards step, one
//! observed jump was 60 seconds — so ordering on anything inside a payload is
//! already ruled out.
//!
//! An `Instant` has no meaning outside the process that produced it, though, so
//! it cannot be written to a file. That is what this module is for:
//!
//! * [`RecordedEvent::monotonic_offset_ms`] is the **ordering key**. It is the
//!   `Instant` delta from the start of the recording, so replay reproduces the
//!   original arrival order and inter-arrival gaps exactly, with none of the
//!   wall clock's hazards.
//! * [`RecordedEvent::wall`] is the **display key**. It exists so a recording
//!   can be labelled "this ran on Tuesday afternoon" and lined up against a
//!   transcript, a commit, or another recording.
//!
//! Both are derived from a single [`RecordingClock`] origin, so `wall` and
//! `monotonic_offset_ms` sort identically — the wall clock is read **once**, at
//! the start of the recording, and never again. A recording therefore cannot
//! contain a backwards timestamp even if the system clock jumps mid-session.
//!
//! # File format
//!
//! JSON Lines, matching the transcript idiom Claude Code already uses and the
//! only format that survives a truncated file:
//!
//! ```text
//! {"format":1,"wall_origin":1756713600000,"producer":"polis 0.1.0"}   <- RecordingHeader
//! {"wall":1756713600123,"monotonic_offset_ms":123,"event":{…}}        <- RecordedEvent
//! {"wall":1756713600456,"monotonic_offset_ms":456,"event":{…}}
//! ```
//!
//! [`RECORDING_FORMAT`] is written into the header on purpose. A wire format
//! without a version field is precisely the thing that is cheap to add now and
//! expensive to retrofit later, which is the whole reason this module exists
//! before M2 rather than during it.
//!
//! **The `Event` enum's variant names are the wire format.** serde's default
//! external tagging writes `{"Hook":{…}}`, so renaming a [`Payload`] or
//! [`crate::OtelEvent`] variant invalidates every recording on disk. Add
//! variants freely — that is what `#[non_exhaustive]` is for (ADR-0048) — but
//! renaming one is a format break and must bump [`RECORDING_FORMAT`].

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::event::{Event, Payload};

/// Version of the recording format. Bump on any incompatible change, including
/// renaming an [`Event`] variant.
pub const RECORDING_FORMAT: u32 = 1;

// ---------------------------------------------------------------------------
// Wall clock
// ---------------------------------------------------------------------------

/// A wall-clock instant, as milliseconds since the Unix epoch (UTC).
///
/// Written out rather than taken from `chrono` or `time` for the same reason
/// [`crate::LogicalPath::layout_seed`] is written out rather than using
/// `DefaultHasher` (ADR-0029): this value is part of a persisted format, and a
/// dependency that changes its serialization or its leap-second handling in a
/// patch release would change the format under it. Milliseconds since the epoch
/// is unambiguous, monotone as an integer, and identical on every platform.
///
/// Signed, so timestamps before 1970 are representable rather than wrapping — a
/// git repository with a rewritten history can and does contain them, and this
/// type is also `polis-repo`'s commit-time type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WallTime(i64);

impl WallTime {
    /// The Unix epoch.
    pub const UNIX_EPOCH: Self = Self(0);

    /// Reads the system clock.
    ///
    /// The **only** place in Polis that should call this is the construction of
    /// a [`RecordingClock`], once per recording. Nothing on the live path, and
    /// nothing that can reach a layout, may read a clock at all (PRD §7.4).
    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    /// Converts from a [`SystemTime`]. Saturates rather than panicking on a
    /// value outside `i64` milliseconds, which is ~292 million years either way.
    pub fn from_system_time(t: SystemTime) -> Self {
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => Self(i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
            Err(e) => Self(
                i64::try_from(e.duration().as_millis())
                    .map_or(i64::MIN, |ms| ms.checked_neg().unwrap_or(i64::MIN)),
            ),
        }
    }

    /// Milliseconds since the Unix epoch.
    #[inline]
    pub fn unix_millis(self) -> i64 {
        self.0
    }

    /// Whole seconds since the Unix epoch, rounding towards negative infinity.
    ///
    /// This is git's timestamp unit (`%ct`), so it is the form `polis-repo`
    /// reads and writes.
    #[inline]
    pub fn unix_seconds(self) -> i64 {
        self.0.div_euclid(1_000)
    }

    /// From milliseconds since the Unix epoch.
    #[inline]
    pub fn from_unix_millis(millis: i64) -> Self {
        Self(millis)
    }

    /// From whole seconds since the Unix epoch — `git log --format=%ct`.
    /// Saturates on overflow.
    pub fn from_unix_seconds(seconds: i64) -> Self {
        Self(seconds.saturating_mul(1_000))
    }

    /// This instant plus a duration. Saturates rather than wrapping.
    #[must_use]
    pub fn saturating_add(self, delta: Duration) -> Self {
        let ms = i64::try_from(delta.as_millis()).unwrap_or(i64::MAX);
        Self(self.0.saturating_add(ms))
    }

    /// Elapsed time from `earlier` to `self`, or `None` if `self` is earlier.
    pub fn duration_since(self, earlier: Self) -> Option<Duration> {
        let delta = self.0.checked_sub(earlier.0)?;
        u64::try_from(delta).ok().map(Duration::from_millis)
    }

    /// Whole days from `self` to `now`, floored, or 0 if `now` precedes `self`.
    ///
    /// PRD §8's overgrowth threshold is 90 days since the last commit, and
    /// PRD §7.5's decay window is the same measurement.
    pub fn days_until(self, now: Self) -> u32 {
        let delta_ms = now.0.saturating_sub(self.0);
        if delta_ms <= 0 {
            return 0;
        }
        u32::try_from(delta_ms / 86_400_000).unwrap_or(u32::MAX)
    }
}

impl From<SystemTime> for WallTime {
    fn from(t: SystemTime) -> Self {
        Self::from_system_time(t)
    }
}

// ---------------------------------------------------------------------------
// The recorded event
// ---------------------------------------------------------------------------

/// One [`Event`] as it appears on disk (ADR-0049).
///
/// Ordering is by [`monotonic_offset_ms`](Self::monotonic_offset_ms), never by
/// [`wall`](Self::wall) and never by anything inside the payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedEvent {
    /// Wall-clock time of arrival, for display and for lining a recording up
    /// against a transcript or a commit. Derived from the recording origin plus
    /// the monotonic offset, so it inherits the monotonic clock's ordering.
    pub wall: WallTime,
    /// Milliseconds since the recording started, from the monotonic clock. This
    /// is the ordering key and the replay schedule.
    pub monotonic_offset_ms: u64,
    /// The event itself. Its [`crate::EventMeta::observed`] is not serialized — the two
    /// fields above replace it (see the module docs).
    pub event: Event,
}

impl RecordedEvent {
    /// The channel this event came from, without unwrapping the payload.
    pub fn channel(&self) -> crate::event::Channel {
        self.event.meta.channel
    }

    /// The payload, for a caller that only wants to filter a recording.
    pub fn payload(&self) -> &Payload {
        &self.event.payload
    }

    /// Serializes to one JSON Lines record, newline excluded.
    pub fn to_json_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Parses one JSON Lines record.
    ///
    /// A parse failure is a **skipped line**, never a fatal error: PRD §4.4's
    /// rule that one bad line must not abort a file applies to Polis's own
    /// recordings too, which are appended to while they are being read.
    pub fn from_json_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }
}

/// The first line of a recording file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingHeader {
    /// [`RECORDING_FORMAT`] at the time of writing. A reader that does not
    /// recognise the value must refuse the file rather than guess at it.
    pub format: u32,
    /// Wall-clock time of the recording's origin. Every
    /// [`RecordedEvent::monotonic_offset_ms`] is relative to this.
    pub wall_origin: WallTime,
    /// Producer identification, e.g. `"polis 0.1.0"`. Free-form; for the
    /// operator, not for dispatch.
    pub producer: String,
}

impl RecordingHeader {
    /// The header for a clock, tagging the file with this crate's version.
    pub fn for_clock(clock: RecordingClock) -> Self {
        Self {
            format: RECORDING_FORMAT,
            wall_origin: clock.wall_origin(),
            producer: concat!("polis-events ", env!("CARGO_PKG_VERSION")).to_owned(),
        }
    }

    /// True when this reader can interpret the file.
    pub fn is_supported(&self) -> bool {
        self.format == RECORDING_FORMAT
    }
}

// ---------------------------------------------------------------------------
// Live <-> recorded
// ---------------------------------------------------------------------------

/// Converts live [`Event`]s into [`RecordedEvent`]s (the writing half).
///
/// Holds the single wall-clock reading a recording is allowed to make, paired
/// with the [`Instant`] it was taken at. Everything after that is arithmetic on
/// the monotonic clock.
#[derive(Debug, Clone, Copy)]
pub struct RecordingClock {
    wall_origin: WallTime,
    mono_origin: Instant,
}

impl Default for RecordingClock {
    fn default() -> Self {
        Self::start_now()
    }
}

impl RecordingClock {
    /// Starts a recording now. Reads the system clock exactly once.
    pub fn start_now() -> Self {
        Self {
            wall_origin: WallTime::now(),
            mono_origin: Instant::now(),
        }
    }

    /// Starts a recording at an explicit origin. Tests use this to get a
    /// deterministic wall clock; nothing else should.
    pub fn start_at(wall_origin: WallTime, mono_origin: Instant) -> Self {
        Self {
            wall_origin,
            mono_origin,
        }
    }

    /// The wall clock at the recording's origin.
    #[inline]
    pub fn wall_origin(self) -> WallTime {
        self.wall_origin
    }

    /// The monotonic clock at the recording's origin.
    #[inline]
    pub fn mono_origin(self) -> Instant {
        self.mono_origin
    }

    /// The offset an event would be recorded at.
    ///
    /// Saturates at zero for an [`Instant`] before the origin, which is possible
    /// when a channel started before recording did.
    pub fn offset_ms(self, observed: Instant) -> u64 {
        let d = observed.saturating_duration_since(self.mono_origin);
        u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
    }

    /// Records an event, consuming it.
    pub fn record(self, event: Event) -> RecordedEvent {
        let monotonic_offset_ms = self.offset_ms(event.meta.observed);
        RecordedEvent {
            wall: self
                .wall_origin
                .saturating_add(Duration::from_millis(monotonic_offset_ms)),
            monotonic_offset_ms,
            event,
        }
    }

    /// Records an event without consuming it.
    pub fn record_ref(self, event: &Event) -> RecordedEvent {
        self.record(event.clone())
    }
}

/// Converts [`RecordedEvent`]s back into live [`Event`]s (the reading half).
///
/// Rebases each event's [`crate::EventMeta::observed`] onto a fresh monotonic origin,
/// so a replayed stream is orderable and windowable by exactly the same rules as
/// a live one — PRD §4.3's ±2 s correlation window works unchanged, which is the
/// point of replay being a first-class mode rather than a debug tool.
#[derive(Debug, Clone, Copy)]
pub struct ReplayClock {
    mono_origin: Instant,
}

impl Default for ReplayClock {
    fn default() -> Self {
        Self::start_now()
    }
}

impl ReplayClock {
    /// Rebases onto now.
    pub fn start_now() -> Self {
        Self {
            mono_origin: Instant::now(),
        }
    }

    /// Rebases onto an explicit origin.
    pub fn starting_at(mono_origin: Instant) -> Self {
        Self { mono_origin }
    }

    /// The monotonic origin replayed events are stamped relative to.
    #[inline]
    pub fn mono_origin(self) -> Instant {
        self.mono_origin
    }

    /// Restores a live event.
    ///
    /// The returned [`crate::EventMeta::observed`] is `mono_origin + offset`, so the
    /// gaps between replayed events match the gaps in the recording.
    pub fn live(self, recorded: RecordedEvent) -> Event {
        let RecordedEvent {
            monotonic_offset_ms,
            mut event,
            ..
        } = recorded;
        event.meta.observed = self.mono_origin + Duration::from_millis(monotonic_offset_ms);
        event
    }
}

/// A recorded event's live form, with `observed` stamped relative to `origin`.
///
/// The free-function form of [`ReplayClock::live`], for a caller that has an
/// origin but no reason to keep a clock around.
pub fn to_live(recorded: RecordedEvent, origin: Instant) -> Event {
    ReplayClock::starting_at(origin).live(recorded)
}

/// The recorded form of a live event, relative to `clock`.
///
/// The free-function form of [`RecordingClock::record`].
pub fn to_recorded(event: Event, clock: RecordingClock) -> RecordedEvent {
    clock.record(event)
}

/// A [`Payload`]-only summary for a caller filtering a recording without
/// deserializing a whole `Event`.
///
/// Kept as a function rather than a method on [`crate::EventMeta`] because it is a
/// property of the recording, not of the event.
pub fn is_control(recorded: &RecordedEvent) -> bool {
    matches!(recorded.event.payload, Payload::Control(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Channel, ControlEvent, EventMeta, FsEvent};
    use crate::ids::{SessionId, WorktreeId};
    use crate::path::LogicalPath;

    fn fs_event(path: &str) -> Payload {
        Payload::Fs(FsEvent::Modified {
            path: (WorktreeId::PRIMARY, LogicalPath::new(path).unwrap()),
        })
    }

    #[test]
    fn a_recorded_event_round_trips_through_json() {
        let clock = RecordingClock::start_at(
            WallTime::from_unix_millis(1_756_713_600_000),
            Instant::now(),
        );
        let meta = EventMeta::now(Channel::Fs).with_session(SessionId::new("s1"));
        let recorded = clock.record(Event::new(meta, fs_event("src/main.rs")));

        let line = recorded.to_json_line().expect("serializes");
        assert!(!line.contains('\n'), "a JSON Lines record is one line");
        let back = RecordedEvent::from_json_line(&line).expect("parses");

        assert_eq!(back.wall, recorded.wall);
        assert_eq!(back.monotonic_offset_ms, recorded.monotonic_offset_ms);
        assert_eq!(back.channel(), Channel::Fs);
        assert_eq!(
            back.event.meta.session.as_ref().map(SessionId::as_str),
            Some("s1")
        );
        match back.payload() {
            Payload::Fs(FsEvent::Modified { path }) => {
                assert_eq!(path.0, WorktreeId::PRIMARY);
                assert_eq!(path.1.as_str(), "src/main.rs");
            }
            other => panic!("payload changed shape: {other:?}"),
        }
    }

    #[test]
    fn replay_preserves_order_and_inter_arrival_gaps() {
        let mono_origin = Instant::now();
        let clock = RecordingClock::start_at(WallTime::from_unix_millis(0), mono_origin);

        // Three events 0 ms, 40 ms and 900 ms after the origin.
        let offsets = [0_u64, 40, 900];
        let recorded: Vec<RecordedEvent> = offsets
            .iter()
            .map(|&ms| {
                let mut meta = EventMeta::now(Channel::Fs);
                meta.observed = mono_origin + Duration::from_millis(ms);
                clock.record(Event::new(meta, fs_event("a.rs")))
            })
            .collect();

        for (r, &ms) in recorded.iter().zip(offsets.iter()) {
            assert_eq!(r.monotonic_offset_ms, ms);
            assert_eq!(r.wall.unix_millis(), i64::try_from(ms).unwrap());
        }

        // Serialize, parse, rebase onto a *different* origin.
        let lines: Vec<String> = recorded.iter().map(|r| r.to_json_line().unwrap()).collect();
        let replay_origin = Instant::now();
        let replay = ReplayClock::starting_at(replay_origin);
        let live: Vec<Event> = lines
            .iter()
            .map(|l| replay.live(RecordedEvent::from_json_line(l).unwrap()))
            .collect();

        assert!(
            live[0].meta.observed < live[1].meta.observed
                && live[1].meta.observed < live[2].meta.observed,
            "replay must preserve arrival order"
        );
        assert_eq!(
            live[2].meta.observed - live[1].meta.observed,
            Duration::from_millis(860),
            "and the gaps between arrivals"
        );
        assert_eq!(live[0].meta.observed, replay_origin);
    }

    #[test]
    fn wall_and_monotonic_offset_sort_identically() {
        // The wall clock is read once, at the origin, so a system-clock jump
        // during a recording cannot reorder the file.
        let mono_origin = Instant::now();
        let clock = RecordingClock::start_at(WallTime::from_unix_millis(1_000), mono_origin);
        let mut prev: Option<RecordedEvent> = None;
        for ms in [0_u64, 1, 17, 5_000, 5_001] {
            let mut meta = EventMeta::now(Channel::Otel);
            meta.observed = mono_origin + Duration::from_millis(ms);
            let r = clock.record(Event::new(meta, fs_event("b.rs")));
            if let Some(p) = prev {
                assert!(p.monotonic_offset_ms <= r.monotonic_offset_ms);
                assert!(p.wall <= r.wall, "wall must not disagree with the offset");
            }
            prev = Some(r);
        }
    }

    #[test]
    fn an_event_before_the_origin_saturates_instead_of_panicking() {
        let mono_origin = Instant::now();
        let clock = RecordingClock::start_at(WallTime::from_unix_millis(500), mono_origin);
        let mut meta = EventMeta::now(Channel::Hook);
        // A channel that started before recording did.
        meta.observed = mono_origin
            .checked_sub(Duration::from_secs(30))
            .unwrap_or(mono_origin);
        let r = clock.record(Event::new(meta, fs_event("c.rs")));
        assert_eq!(r.monotonic_offset_ms, 0);
        assert_eq!(r.wall.unix_millis(), 500);
    }

    #[test]
    fn the_header_carries_a_format_version() {
        let clock = RecordingClock::start_at(WallTime::from_unix_millis(42), Instant::now());
        let h = RecordingHeader::for_clock(clock);
        assert!(h.is_supported());
        assert_eq!(h.wall_origin.unix_millis(), 42);

        let json = serde_json::to_string(&h).unwrap();
        let back: RecordingHeader = serde_json::from_str(&json).unwrap();
        assert_eq!(back.format, RECORDING_FORMAT);

        // A file from a future Polis is refused, not misread.
        let future = r#"{"format":9999,"wall_origin":0,"producer":"polis 9.0"}"#;
        let h: RecordingHeader = serde_json::from_str(future).unwrap();
        assert!(!h.is_supported());
    }

    #[test]
    fn control_events_survive_a_round_trip() {
        let clock = RecordingClock::start_now();
        let r = clock.record(Event::control(ControlEvent::SequenceGap {
            session: SessionId::new("s2"),
            expected: 7,
            got: 9,
        }));
        assert!(is_control(&r));
        let back = RecordedEvent::from_json_line(&r.to_json_line().unwrap()).unwrap();
        match back.payload() {
            Payload::Control(ControlEvent::SequenceGap {
                session,
                expected,
                got,
            }) => {
                assert_eq!(session.as_str(), "s2");
                assert_eq!((*expected, *got), (7, 9));
            }
            other => panic!("control payload changed shape: {other:?}"),
        }
    }

    #[test]
    fn wall_time_arithmetic_is_total() {
        assert_eq!(WallTime::UNIX_EPOCH.unix_millis(), 0);
        assert_eq!(WallTime::from_unix_seconds(90).unix_millis(), 90_000);
        assert_eq!(WallTime::from_unix_millis(1_500).unix_seconds(), 1);
        // Floors towards negative infinity, so a pre-epoch commit does not round
        // the wrong way.
        assert_eq!(WallTime::from_unix_millis(-1_500).unix_seconds(), -2);

        let t0 = WallTime::from_unix_seconds(0);
        let t1 = WallTime::from_unix_seconds(90 * 86_400);
        assert_eq!(t0.days_until(t1), 90, "PRD §8's overgrowth threshold");
        assert_eq!(
            t1.days_until(t0),
            0,
            "a future timestamp is not negative age"
        );
        assert_eq!(t0.duration_since(t1), None);
        assert_eq!(
            t1.duration_since(t0).map(|d| d.as_secs()),
            Some(90 * 86_400)
        );

        // Saturation, not wrapping.
        let far = WallTime::from_unix_millis(i64::MAX);
        assert_eq!(
            far.saturating_add(Duration::from_secs(1)).unix_millis(),
            i64::MAX
        );
        assert_eq!(
            WallTime::from_unix_seconds(i64::MAX).unix_millis(),
            i64::MAX
        );
    }

    #[test]
    fn system_time_round_trips_within_a_millisecond() {
        let now = SystemTime::now();
        let w = WallTime::from_system_time(now);
        let elapsed = now.duration_since(UNIX_EPOCH).unwrap();
        assert_eq!(w.unix_millis(), i64::try_from(elapsed.as_millis()).unwrap());
        // Pre-epoch is representable.
        let before = UNIX_EPOCH - Duration::from_secs(3);
        assert_eq!(WallTime::from_system_time(before).unix_millis(), -3_000);
    }

    /// A recording is read while it is still being appended to, so a half-written
    /// final line must be a skipped line rather than an aborted file.
    #[test]
    fn a_truncated_line_is_an_error_not_a_panic() {
        for line in ["", "{", "null", "[]", "{\"wall\":", "{\"wall\":1}"] {
            assert!(RecordedEvent::from_json_line(line).is_err(), "{line:?}");
        }
    }
}
