//! The offline transcript driver — M2's engine (PRD §15).
//!
//! > **M2 — Single-session replay.** Read one JSONL file offline and animate it
//! > over the city. This is the fastest possible loop for iterating on the
//! > visual language — minutes per iteration, real data, no live infrastructure.
//! > […] Spend real time here; most of the notation gets decided in this
//! > milestone.
//!
//! # Two timelines, and why both are needed
//!
//! | Timeline | Unit | What it is |
//! |---|---|---|
//! | **session** | [`polis_events::RecordedEvent::monotonic_offset_ms`] | the real inter-arrival gaps, hours long, with 40-minute holes |
//! | **schedule** | [`ScheduledEvent::schedule_ms`] | what the operator watches, with idle gaps capped |
//!
//! Real sessions are mostly dead air. A four-hour transcript with six bursts of
//! work in it is unwatchable at 1× and dishonest at 64×, because 64× also
//! compresses the bursts. So [`ReplaySchedule::compress_idle_gaps`] caps any
//! single gap at [`DEFAULT_IDLE_GAP_CAP`] — **2 seconds**, chosen because it is
//! long enough to read as "time passed" and short enough that six of them cost
//! twelve seconds instead of three hours — and leaves the bursts at their real
//! speed. The cap is configurable and can be turned off entirely.
//!
//! The world is still driven on **session** time. A compressed 40-minute gap
//! ages the world by 40 minutes over 2 seconds of watching, so trails fade,
//! claims expire and territories decay exactly as they did — quickly, but
//! visibly, rather than jumping. That mapping is
//! [`ReplaySchedule::session_ms_at`].
//!
//! # Ordering
//!
//! Transcript timestamps step backwards on 20% of files and one observed jump
//! was 60 seconds (ADR-0014), so `polis-ingest` writes a running maximum in byte
//! order and this module **sorts defensively anyway** with a stable sort: a
//! recording produced by anything else has no such guarantee, and an unordered
//! schedule would replay a session inside out.
//!
//! # `ReplayClock` here is not [`polis_events::ReplayClock`]
//!
//! The one in `polis-events` is a *rebasing* helper: it puts a coherent
//! [`Instant`] back on a recorded event. [`ReplayClock`] here is the
//! **transport** — play, pause, speed, seek. They compose: the transport decides
//! *when* an event is applied, the rebasing decides what [`Instant`] it carries.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use polis_events::{
    Channel, EventKind, LogicalPath, Outcome, PathMapper, Payload, RecordedEvent, RecordingHeader,
    ToolKind, TranscriptRecordKind, TranscriptSource, WallTime,
};
use polis_ingest::transcript::{
    read_session, read_transcript, transcript_source, ParseStats, Replay, TranscriptFile,
};
use serde_json::Value;

use crate::World;

/// The default cap on a single idle gap.
///
/// > Real sessions have long dead air between bursts.
///
/// 2 s reads as "time passed here" without being time the operator has to spend.
/// Set it with [`ReplaySchedule::compress_idle_gaps`], or pass `None` to watch a
/// session at its true pacing.
pub const DEFAULT_IDLE_GAP_CAP: Duration = Duration::from_secs(2);

/// A gap in **session** time long enough to count as a new burst of work.
///
/// The boundary [`Interest::Burst`] marks, so "jump to the next interesting
/// moment" lands on the start of the next burst rather than in the middle of the
/// silence before it.
pub const BURST_GAP: Duration = Duration::from_secs(30);

/// Slowest and fastest playback.
pub const SPEED_RANGE: (f32, f32) = (0.25, 64.0);

// ---------------------------------------------------------------------------
// The schedule
// ---------------------------------------------------------------------------

/// One event, placed on both timelines.
#[derive(Debug, Clone)]
pub struct ScheduledEvent {
    /// Position on the playback timeline, in milliseconds.
    pub schedule_ms: u64,
    /// The event, whose `monotonic_offset_ms` is its position on the session
    /// timeline.
    pub event: RecordedEvent,
    /// Why a scrubber might want to stop here.
    pub interest: Option<Interest>,
}

impl ScheduledEvent {
    /// Position on the session timeline, in milliseconds.
    pub fn session_ms(&self) -> u64 {
        self.event.monotonic_offset_ms
    }

    /// Wall-clock time, for a label. Display only (ADR-0014).
    pub fn wall(&self) -> WallTime {
        self.event.wall
    }
}

/// A time-ordered schedule of one session's events.
#[derive(Debug, Clone)]
pub struct ReplaySchedule {
    /// Format version and wall-clock origin, from the reader.
    pub header: RecordingHeader,
    /// Events, ordered by session time.
    pub entries: Vec<ScheduledEvent>,
    /// What the transcript parser skipped and why.
    pub stats: ParseStats,
    /// The transcript files that were read, in read order.
    pub files: Vec<TranscriptFile>,
    /// The idle-gap cap in force, if any.
    pub idle_gap_cap: Option<Duration>,
}

impl ReplaySchedule {
    /// Reads a whole session offline: main transcript plus every subagent file.
    ///
    /// This is `polis-ingest`'s OFFLINE FULL READ. Files are read in
    /// `(file, byte_offset)` order with the main transcript first, so the
    /// `Agent` `tool_use` that spawned a subagent is always seen before the
    /// subagent's own records — which is what lets the world place the worker
    /// instead of parking it.
    pub fn from_session_dir(dir: &Path, mapper: &PathMapper) -> io::Result<Self> {
        Ok(Self::from_replay(read_session(dir, mapper)?))
    }

    /// Reads one transcript file offline.
    pub fn from_transcript(path: &Path, mapper: &PathMapper) -> io::Result<Self> {
        Ok(Self::from_replay(read_transcript(path, mapper)?))
    }

    /// Reads whatever the path is: a session sidecar directory, or one file.
    ///
    /// A main transcript at `<munged-cwd>/<session-id>.jsonl` has its subagents
    /// in the sibling directory `<munged-cwd>/<session-id>/`, so this reads both
    /// when it can find them — 79% of transcript files in a real corpus are
    /// subagent files, and a replay that skipped them would show an orchestrator
    /// doing nothing (ADR-0013).
    pub fn open(path: &Path, mapper: &PathMapper) -> io::Result<Self> {
        if path.is_dir() {
            return Self::from_session_dir(path, mapper);
        }
        match transcript_source(path) {
            // `session_files` takes the `<session-id>` sidecar directory, which
            // need not exist, and finds the main transcript beside it.
            Some(TranscriptSource::Main) => {
                Self::from_session_dir(&path.with_extension(""), mapper)
            }
            _ => Self::from_transcript(path, mapper),
        }
    }

    /// Wraps an already-read [`Replay`].
    pub fn from_replay(replay: Replay) -> Self {
        let Replay {
            header,
            events,
            stats,
            files,
        } = replay;
        Self::from_events(header, events, stats, files)
    }

    /// Builds a schedule from recorded events, sorting defensively.
    pub fn from_events(
        header: RecordingHeader,
        mut events: Vec<RecordedEvent>,
        stats: ParseStats,
        files: Vec<TranscriptFile>,
    ) -> Self {
        // Stable, so events that share a millisecond keep the order the reader
        // gave them — which for a transcript is byte order, the only true append
        // order there is (ADR-0014).
        events.sort_by_key(|e| e.monotonic_offset_ms);
        let mut previous: Option<u64> = None;
        let entries = events
            .into_iter()
            .map(|event| {
                let gap = previous.map(|p| event.monotonic_offset_ms.saturating_sub(p));
                previous = Some(event.monotonic_offset_ms);
                let interest = classify(&event, gap);
                ScheduledEvent {
                    schedule_ms: event.monotonic_offset_ms,
                    event,
                    interest,
                }
            })
            .collect();
        Self {
            header,
            entries,
            stats,
            files,
            idle_gap_cap: None,
        }
    }

    /// Caps every gap on the playback timeline.
    ///
    /// `None` restores the session's true pacing. This rewrites only
    /// [`ScheduledEvent::schedule_ms`]; the session timeline is untouched, so
    /// the world still ages by the real elapsed time.
    pub fn compress_idle_gaps(&mut self, cap: Option<Duration>) {
        self.idle_gap_cap = cap;
        let cap_ms = cap.map_or(u64::MAX, |c| {
            u64::try_from(c.as_millis()).unwrap_or(u64::MAX)
        });
        let mut previous_session: Option<u64> = None;
        let mut cursor = 0_u64;
        for entry in &mut self.entries {
            let session = entry.event.monotonic_offset_ms;
            let gap = previous_session.map_or(0, |p| session.saturating_sub(p));
            cursor = cursor.saturating_add(gap.min(cap_ms));
            entry.schedule_ms = cursor;
            previous_session = Some(session);
        }
    }

    /// The same, as a builder.
    #[must_use]
    pub fn with_idle_gap_cap(mut self, cap: Option<Duration>) -> Self {
        self.compress_idle_gaps(cap);
        self
    }

    /// How long the playback timeline is.
    pub fn duration_ms(&self) -> u64 {
        self.entries.last().map_or(0, |e| e.schedule_ms)
    }

    /// How long the session actually was.
    pub fn session_duration_ms(&self) -> u64 {
        self.entries.last().map_or(0, ScheduledEvent::session_ms)
    }

    /// How many events there are.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing parsed.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How much dead air the cap removed, in milliseconds.
    pub fn compressed_ms(&self) -> u64 {
        self.session_duration_ms()
            .saturating_sub(self.duration_ms())
    }

    /// The session time a point on the playback timeline corresponds to.
    ///
    /// Piecewise-linear across the entries, so a compressed gap plays back as an
    /// accelerated stretch of session time rather than as a jump: 40 minutes of
    /// decay happens over the capped 2 seconds, visibly.
    pub fn session_ms_at(&self, schedule_ms: u64) -> u64 {
        if self.entries.is_empty() {
            return schedule_ms;
        }
        let idx = self
            .entries
            .partition_point(|e| e.schedule_ms <= schedule_ms);
        if idx == 0 {
            return self.entries[0].session_ms();
        }
        let prev = &self.entries[idx - 1];
        let Some(next) = self.entries.get(idx) else {
            // Past the last event: session time runs on at real speed.
            return prev.session_ms() + (schedule_ms - prev.schedule_ms);
        };
        let span = next.schedule_ms.saturating_sub(prev.schedule_ms);
        if span == 0 {
            return prev.session_ms();
        }
        let into = schedule_ms.saturating_sub(prev.schedule_ms);
        let session_span = next.session_ms().saturating_sub(prev.session_ms());
        prev.session_ms() + (session_span.saturating_mul(into) / span)
    }

    /// The index of the first event at or after a point on the playback
    /// timeline.
    pub fn index_at(&self, schedule_ms: u64) -> usize {
        self.entries
            .partition_point(|e| e.schedule_ms < schedule_ms)
    }

    /// Every moment a scrubber might want to stop at, in order.
    pub fn interesting(&self) -> impl Iterator<Item = (usize, Interest)> + '_ {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.interest.map(|k| (i, k)))
    }
}

/// Why a moment is worth stopping at.
///
/// Computed from the event alone, at schedule-build time, so
/// [`ReplayDriver::seek_next_interesting`] is a lookup rather than a simulation.
/// Contention is deliberately **not** here: it is a relation between two threads
/// and cannot be known without the world, so it is found by watching, which is
/// the point of watching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interest {
    /// A thread parked on a human — PRD §11.2 state (a), the primary state.
    Decision,
    /// A subagent was spawned. The fan-out moments are where a session's shape
    /// is decided.
    Delegation,
    /// A tool call failed, was rejected, or the API errored.
    Failure,
    /// A thread or a subagent finished.
    Finish,
    /// The first event after [`BURST_GAP`] of silence — the start of the next
    /// burst of work.
    Burst,
}

impl Interest {
    /// An operator-facing label for a scrubber tick.
    pub fn label(self) -> &'static str {
        match self {
            Self::Decision => "waiting on you",
            Self::Delegation => "delegated",
            Self::Failure => "failed",
            Self::Finish => "finished",
            Self::Burst => "work resumed",
        }
    }
}

/// Classifies one event, given the session-time gap before it.
fn classify(event: &RecordedEvent, gap: Option<u64>) -> Option<Interest> {
    if let Some(gap) = gap {
        if Duration::from_millis(gap) >= BURST_GAP {
            return Some(Interest::Burst);
        }
    }
    match &event.event.payload {
        Payload::Hook(h) => match h.kind {
            EventKind::PermissionRequest
            | EventKind::Elicitation
            | EventKind::TeammateIdle
            | EventKind::Notification => Some(Interest::Decision),
            EventKind::SubagentStart => Some(Interest::Delegation),
            EventKind::Stop | EventKind::SessionEnd | EventKind::SubagentStop => {
                Some(Interest::Finish)
            }
            EventKind::PostToolUseFailure | EventKind::StopFailure => Some(Interest::Failure),
            _ => None,
        },
        Payload::Otel(e) => match &**e {
            polis_events::OtelEvent::ApiError { .. }
            | polis_events::OtelEvent::ApiRefusal
            | polis_events::OtelEvent::ApiRetriesExhausted => Some(Interest::Failure),
            polis_events::OtelEvent::SubagentCompleted { .. } => Some(Interest::Finish),
            polis_events::OtelEvent::ToolResult(call) if call.outcome == Outcome::Failed => {
                Some(Interest::Failure)
            }
            polis_events::OtelEvent::ToolSpan { call, .. } if call.tool.spawns_worker() => {
                Some(Interest::Delegation)
            }
            _ => None,
        },
        Payload::Transcript(t) => transcript_interest(&t.kind, &t.record),
        _ => None,
    }
}

/// Classifies a transcript record without deserializing the whole thing.
fn transcript_interest(kind: &TranscriptRecordKind, record: &Value) -> Option<Interest> {
    let blocks = record
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)?;
    match *kind {
        TranscriptRecordKind::Assistant => blocks
            .iter()
            .any(|b| {
                b.get("type").and_then(Value::as_str) == Some("tool_use")
                    && b.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| ToolKind::parse(n).spawns_worker())
            })
            .then_some(Interest::Delegation),
        TranscriptRecordKind::User => blocks
            .iter()
            .any(|b| {
                b.get("type").and_then(Value::as_str) == Some("tool_result")
                    && b.get("is_error").and_then(Value::as_bool) == Some(true)
            })
            .then_some(Interest::Failure),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The transport
// ---------------------------------------------------------------------------

/// Play, pause, speed and seek over a schedule.
///
/// Distinct from [`polis_events::ReplayClock`], which rebases a recorded event
/// onto a fresh monotonic origin. This one decides *when*; that one decides
/// *what instant the event carries*.
#[derive(Debug, Clone)]
pub struct ReplayClock {
    playing: bool,
    speed: f32,
    position_ms: f64,
    duration_ms: u64,
}

impl ReplayClock {
    /// A paused clock at the start of a timeline `duration_ms` long.
    pub fn new(duration_ms: u64) -> Self {
        Self {
            playing: false,
            speed: 1.0,
            position_ms: 0.0,
            duration_ms,
        }
    }

    /// Starts playing.
    pub fn play(&mut self) {
        self.playing = true;
    }

    /// Stops playing. The position is kept.
    pub fn pause(&mut self) {
        self.playing = false;
    }

    /// Toggles play/pause and reports the new state.
    pub fn toggle(&mut self) -> bool {
        self.playing = !self.playing;
        self.playing
    }

    /// Whether the clock is running.
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Playback speed, in `0.25 ..= 64`.
    pub fn speed(&self) -> f32 {
        self.speed
    }

    /// Sets the playback speed, clamped to [`SPEED_RANGE`].
    pub fn set_speed(&mut self, speed: f32) {
        self.speed = if speed.is_finite() {
            speed.clamp(SPEED_RANGE.0, SPEED_RANGE.1)
        } else {
            1.0
        };
    }

    /// Doubles the speed, up to the cap.
    pub fn faster(&mut self) {
        self.set_speed(self.speed * 2.0);
    }

    /// Halves the speed, down to the floor.
    pub fn slower(&mut self) {
        self.set_speed(self.speed / 2.0);
    }

    /// Position on the playback timeline, in milliseconds.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // `position_ms` is clamped to `[0, duration_ms]`, so the cast is exact.
    pub fn position_ms(&self) -> u64 {
        self.position_ms.max(0.0) as u64
    }

    /// How long the playback timeline is.
    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    /// Jumps to a point on the playback timeline.
    pub fn seek(&mut self, schedule_ms: u64) {
        #[allow(clippy::cast_precision_loss)]
        // milliseconds of a session; exact well past f64's 2^53
        let target = schedule_ms.min(self.duration_ms) as f64;
        self.position_ms = target;
    }

    /// Jumps to a fraction of the way through.
    pub fn seek_fraction(&mut self, fraction: f32) {
        #[allow(clippy::cast_precision_loss)]
        let total = self.duration_ms as f64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let target = (total * f64::from(fraction.clamp(0.0, 1.0))) as u64;
        self.seek(target);
    }

    /// Advances by real elapsed time, scaled by the speed, and returns the span
    /// crossed as `(from_ms, to_ms)`.
    ///
    /// A paused clock advances nothing and returns an empty span, so a render
    /// loop can call this unconditionally.
    pub fn advance(&mut self, real: Duration) -> (u64, u64) {
        let from = self.position_ms();
        if !self.playing {
            return (from, from);
        }
        self.position_ms += real.as_secs_f64() * f64::from(self.speed) * 1000.0;
        #[allow(clippy::cast_precision_loss)]
        let limit = self.duration_ms as f64;
        if self.position_ms >= limit {
            self.position_ms = limit;
            self.playing = false;
        }
        (from, self.position_ms())
    }

    /// True once the clock has reached the end.
    pub fn is_finished(&self) -> bool {
        self.position_ms() >= self.duration_ms
    }

    /// Progress as a fraction in `[0, 1]`.
    #[allow(clippy::cast_possible_truncation)] // a value already clamped to [0, 1]
    pub fn fraction(&self) -> f32 {
        if self.duration_ms == 0 {
            return 1.0;
        }
        #[allow(clippy::cast_precision_loss)] // milliseconds; exact well past 2^53
        let f = self.position_ms / self.duration_ms as f64;
        f.clamp(0.0, 1.0) as f32
    }
}

/// A schedule, a clock, and the cursor between them.
///
/// This is what a window drives: call [`ReplayDriver::advance`] once per frame
/// with the frame's delta, then read the snapshot. Nothing here renders, and
/// nothing here reads a wall clock except through the caller's `dt`.
#[derive(Debug)]
pub struct ReplayDriver {
    schedule: ReplaySchedule,
    clock: ReplayClock,
    /// Index of the next event to apply.
    cursor: usize,
    /// The [`Instant`] session-time zero maps to.
    origin: Instant,
    applied: usize,
}

impl ReplayDriver {
    /// A driver over a schedule, paused at the start.
    pub fn new(schedule: ReplaySchedule) -> Self {
        Self::with_origin(schedule, Instant::now())
    }

    /// The same, against an explicit monotonic origin.
    ///
    /// Every event's [`polis_events::EventMeta::observed`] is
    /// `origin + session_ms`, so a test can pin the world's whole clock.
    pub fn with_origin(schedule: ReplaySchedule, origin: Instant) -> Self {
        let clock = ReplayClock::new(schedule.duration_ms());
        Self {
            schedule,
            clock,
            cursor: 0,
            origin,
            applied: 0,
        }
    }

    /// The schedule being played.
    pub fn schedule(&self) -> &ReplaySchedule {
        &self.schedule
    }

    /// The transport.
    pub fn clock(&self) -> &ReplayClock {
        &self.clock
    }

    /// The transport, mutably — play, pause, speed.
    ///
    /// Seeking goes through [`ReplayDriver::seek`] rather than through here,
    /// because a backwards seek has to rebuild the world.
    pub fn clock_mut(&mut self) -> &mut ReplayClock {
        &mut self.clock
    }

    /// Advances by real elapsed time and applies everything now due.
    ///
    /// Returns how many events were applied. The world is ticked once, at the
    /// end, with the session time the new position corresponds to — never per
    /// event, so decay does not depend on how chatty the session was.
    pub fn advance(&mut self, real: Duration, world: &mut World) -> usize {
        let (_, to) = self.clock.advance(real);
        let applied = self.apply_through(to, world);
        world.tick(self.session_instant(to));
        applied
    }

    /// Applies exactly one event, wherever the clock is, and moves the clock to
    /// it.
    ///
    /// Returns `false` at the end of the schedule.
    pub fn step(&mut self, world: &mut World) -> bool {
        let Some(entry) = self.schedule.entries.get(self.cursor) else {
            return false;
        };
        let schedule_ms = entry.schedule_ms;
        self.apply_one(self.cursor, world);
        self.cursor += 1;
        self.clock.seek(schedule_ms);
        world.tick(self.session_instant(schedule_ms));
        true
    }

    /// Jumps to a point on the playback timeline.
    ///
    /// A **backwards** seek resets the world and re-applies from the start:
    /// events are not invertible, and re-applying a real session takes
    /// microseconds per event. A forward seek just applies the span.
    pub fn seek(&mut self, schedule_ms: u64, world: &mut World) {
        let target = schedule_ms.min(self.schedule.duration_ms());
        if target < self.clock.position_ms() {
            world.reset(self.origin);
            self.cursor = 0;
            self.applied = 0;
        }
        self.clock.seek(target);
        self.apply_through(target, world);
        world.tick(self.session_instant(target));
    }

    /// Jumps to a fraction of the way through.
    pub fn seek_fraction(&mut self, fraction: f32, world: &mut World) {
        #[allow(clippy::cast_precision_loss)]
        let total = self.schedule.duration_ms() as f64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let target = (total * f64::from(fraction.clamp(0.0, 1.0))) as u64;
        self.seek(target, world);
    }

    /// The next moment worth stopping at, without moving.
    pub fn next_interesting(&self) -> Option<(usize, Interest)> {
        self.schedule
            .entries
            .iter()
            .enumerate()
            .skip(self.cursor)
            .find_map(|(i, e)| e.interest.map(|k| (i, k)))
    }

    /// Jumps to the next moment worth stopping at, applying everything in
    /// between.
    ///
    /// This is what makes a four-hour session navigable: the gaps between bursts
    /// are crossed without being watched, and the world arrives at the moment in
    /// exactly the state it was in.
    pub fn seek_next_interesting(&mut self, world: &mut World) -> Option<Interest> {
        let (index, kind) = self.next_interesting()?;
        let target = self.schedule.entries[index].schedule_ms;
        self.seek(target, world);
        // Land *on* the interesting event, not just before it.
        if self.cursor == index {
            self.step(world);
        }
        Some(kind)
    }

    /// Applies the whole schedule at once, for a headless run.
    pub fn run_to_end(&mut self, world: &mut World) -> usize {
        let end = self.schedule.duration_ms();
        let applied = self.apply_through(end, world);
        self.clock.seek(end);
        world.tick(self.session_instant(end));
        applied
    }

    /// True once every event has been applied.
    pub fn is_finished(&self) -> bool {
        self.cursor >= self.schedule.entries.len()
    }

    /// Everything a scrubber needs to draw itself.
    pub fn progress(&self) -> ReplayProgress {
        let position_ms = self.clock.position_ms();
        ReplayProgress {
            position_ms,
            duration_ms: self.schedule.duration_ms(),
            session_ms: self.schedule.session_ms_at(position_ms),
            session_duration_ms: self.schedule.session_duration_ms(),
            events_applied: self.applied,
            events_total: self.schedule.entries.len(),
            speed: self.clock.speed(),
            playing: self.clock.is_playing(),
            wall: self
                .schedule
                .entries
                .get(self.cursor.saturating_sub(1))
                .map(ScheduledEvent::wall),
        }
    }

    /// The [`Instant`] a point on the playback timeline maps to.
    fn session_instant(&self, schedule_ms: u64) -> Instant {
        self.origin + Duration::from_millis(self.schedule.session_ms_at(schedule_ms))
    }

    /// Applies every event up to and including `schedule_ms`.
    fn apply_through(&mut self, schedule_ms: u64, world: &mut World) -> usize {
        let mut applied = 0;
        while let Some(entry) = self.schedule.entries.get(self.cursor) {
            if entry.schedule_ms > schedule_ms {
                break;
            }
            self.apply_one(self.cursor, world);
            self.cursor += 1;
            applied += 1;
        }
        applied
    }

    /// Applies one event with its session-time [`Instant`] stamped on.
    fn apply_one(&mut self, index: usize, world: &mut World) {
        let Some(entry) = self.schedule.entries.get(index) else {
            return;
        };
        let mut event = entry.event.event.clone();
        event.meta.observed = self.origin + Duration::from_millis(entry.event.monotonic_offset_ms);
        world.apply(&event);
        self.applied += 1;
    }
}

/// Everything a scrubber needs (PRD §12).
#[derive(Debug, Clone)]
pub struct ReplayProgress {
    /// Where the transport is, on the playback timeline.
    pub position_ms: u64,
    /// How long the playback timeline is.
    pub duration_ms: u64,
    /// Where that is in the real session.
    pub session_ms: u64,
    /// How long the real session was.
    pub session_duration_ms: u64,
    /// How many events have been applied.
    pub events_applied: usize,
    /// How many there are.
    pub events_total: usize,
    /// Current speed.
    pub speed: f32,
    /// Whether the transport is running.
    pub playing: bool,
    /// Wall-clock time of the last applied event, for a label. Display only.
    pub wall: Option<WallTime>,
}

impl ReplayProgress {
    /// Progress along the playback timeline, in `[0, 1]`.
    #[allow(clippy::cast_possible_truncation)] // a value already clamped to [0, 1]
    pub fn fraction(&self) -> f32 {
        if self.duration_ms == 0 {
            return 1.0;
        }
        #[allow(clippy::cast_precision_loss)] // milliseconds; exact well past 2^53
        let f = self.position_ms as f64 / self.duration_ms as f64;
        f.clamp(0.0, 1.0) as f32
    }

    /// Elapsed playback time.
    pub fn elapsed(&self) -> Duration {
        Duration::from_millis(self.position_ms)
    }

    /// Total playback time.
    pub fn total(&self) -> Duration {
        Duration::from_millis(self.duration_ms)
    }

    /// Elapsed session time — what a clock in the corner of the replay should
    /// show, because it is what actually happened.
    pub fn session_elapsed(&self) -> Duration {
        Duration::from_millis(self.session_ms)
    }
}

/// Every distinct logical path a schedule mentions.
///
/// Handy for `polis replay` to decide which repository the recording belongs to,
/// and for [`polis_repo::corpus::Corpus::record_session`] after a headless run.
pub fn paths_in(schedule: &ReplaySchedule) -> Vec<LogicalPath> {
    let mut out: Vec<LogicalPath> = Vec::new();
    for entry in &schedule.entries {
        if let Payload::Otel(e) = &entry.event.event.payload {
            if let polis_events::OtelEvent::ToolResult(call)
            | polis_events::OtelEvent::ToolSpan { call, .. } = &**e
            {
                for (_, p) in &call.paths {
                    out.push(p.clone());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The channels a schedule actually contains, for the status bar.
pub fn channels_in(schedule: &ReplaySchedule) -> Vec<(Channel, usize)> {
    let mut counts: Vec<(Channel, usize)> = Vec::new();
    for entry in &schedule.entries {
        let channel = entry.event.channel();
        if let Some(slot) = counts.iter_mut().find(|(c, _)| *c == channel) {
            slot.1 += 1;
        } else {
            counts.push((channel, 1));
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_events::{Event, EventMeta, FsEvent, SessionId, WorktreeId, RECORDING_FORMAT};

    fn header() -> RecordingHeader {
        RecordingHeader {
            format: RECORDING_FORMAT,
            wall_origin: WallTime::from_unix_millis(1_756_713_600_000),
            producer: "test".to_owned(),
        }
    }

    fn recorded(offset_ms: u64, path: &str) -> RecordedEvent {
        let meta = EventMeta::now(Channel::Fs).with_session(SessionId::new("s1"));
        let payload = Payload::Fs(FsEvent::Modified {
            path: (WorktreeId::PRIMARY, LogicalPath::new(path).unwrap()),
        });
        RecordedEvent {
            wall: WallTime::from_unix_millis(i64::try_from(offset_ms).unwrap()),
            monotonic_offset_ms: offset_ms,
            event: Event::new(meta, payload),
        }
    }

    fn schedule(offsets: &[u64]) -> ReplaySchedule {
        let events = offsets.iter().map(|o| recorded(*o, "src/a.rs")).collect();
        ReplaySchedule::from_events(header(), events, ParseStats::default(), Vec::new())
    }

    #[test]
    fn events_are_sorted_defensively() {
        // 20% of transcript files contain a backwards timestamp step and one
        // observed jump was 60 s (ADR-0014). A schedule that trusted its input
        // would replay the session inside out.
        let s = schedule(&[900, 0, 400, 100]);
        let order: Vec<u64> = s.entries.iter().map(ScheduledEvent::session_ms).collect();
        assert_eq!(order, vec![0, 100, 400, 900]);
    }

    #[test]
    fn idle_gaps_compress_without_touching_the_session_timeline() {
        // Two bursts separated by 40 minutes of dead air.
        let mut s = schedule(&[0, 100, 200, 2_400_000, 2_400_100]);
        assert_eq!(s.duration_ms(), 2_400_100);
        s.compress_idle_gaps(Some(DEFAULT_IDLE_GAP_CAP));
        assert_eq!(
            s.duration_ms(),
            2_300,
            "the 40-minute hole is capped at two seconds; the 300 ms of real work stays"
        );
        assert_eq!(
            s.session_duration_ms(),
            2_400_100,
            "and the session timeline is untouched"
        );
        assert!(s.compressed_ms() > 2_390_000);

        // Turning it off restores the true pacing.
        s.compress_idle_gaps(None);
        assert_eq!(s.duration_ms(), 2_400_100);
    }

    #[test]
    fn a_compressed_gap_still_ages_the_world_by_the_real_elapsed_time() {
        let s = schedule(&[0, 2_400_000]).with_idle_gap_cap(Some(DEFAULT_IDLE_GAP_CAP));
        // Halfway through the two-second gap is twenty minutes into the silence.
        let mid = s.session_ms_at(1_000);
        assert!(
            (1_150_000..=1_250_000).contains(&mid),
            "session time runs fast through a compressed gap, not still: {mid}"
        );
        assert_eq!(s.session_ms_at(0), 0);
        assert_eq!(s.session_ms_at(2_000), 2_400_000);
    }

    #[test]
    fn the_clock_clamps_speed_to_the_documented_range() {
        let mut c = ReplayClock::new(10_000);
        c.set_speed(1000.0);
        assert!((c.speed() - SPEED_RANGE.1).abs() < f32::EPSILON);
        c.set_speed(0.0);
        assert!((c.speed() - SPEED_RANGE.0).abs() < f32::EPSILON);
        c.set_speed(f32::NAN);
        assert!((c.speed() - 1.0).abs() < f32::EPSILON);
        c.set_speed(1.0);
        c.faster();
        assert!((c.speed() - 2.0).abs() < f32::EPSILON);
        c.slower();
        c.slower();
        assert!((c.speed() - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn a_paused_clock_advances_nothing_and_speed_scales_time() {
        let mut c = ReplayClock::new(10_000);
        assert_eq!(c.advance(Duration::from_secs(1)), (0, 0));
        c.play();
        assert_eq!(c.advance(Duration::from_secs(1)), (0, 1_000));
        c.set_speed(4.0);
        assert_eq!(c.advance(Duration::from_secs(1)), (1_000, 5_000));
        // Reaching the end stops playback rather than running past it.
        c.advance(Duration::from_secs(60));
        assert!(c.is_finished());
        assert!(!c.is_playing());
        assert!((c.fraction() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn interesting_moments_include_the_start_of_the_next_burst() {
        let s = schedule(&[0, 100, 2_400_000]);
        let marks: Vec<(usize, Interest)> = s.interesting().collect();
        assert_eq!(marks, vec![(2, Interest::Burst)]);
        assert_eq!(Interest::Burst.label(), "work resumed");
    }

    #[test]
    fn channels_and_paths_are_summarised_for_the_status_bar() {
        let s = schedule(&[0, 1]);
        assert_eq!(channels_in(&s), vec![(Channel::Fs, 2)]);
        // Channel C events carry no tool call, so no paths are collected from
        // them: attribution-blind by design (PRD §4.3).
        assert!(paths_in(&s).is_empty());
    }
}
