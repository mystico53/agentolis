//! How much world time one recorded frame is allowed to contain (PRD §13).
//!
//! > **Interpolate everything.** Events arrive discretely; tween agent
//! > positions, cloud density, and building heights between updates. Cheap, and
//! > it is the entire difference between "alive" and "steppy."
//!
//! The interpolation was already there and the replays were still steppy,
//! because the thing that decides whether a frame is a tween or a jump cut is
//! **world-seconds per frame**, and that was a caller-supplied constant: the
//! recorder cut a fixed span of the timeline into a fixed number of frames and
//! nothing in the pipeline checked the result. Measured on the three M2
//! recordings, one of them ran at ~0.9 world-seconds per frame and read as
//! alive while another spent 43% of its frames changing fewer than 200 of 1.32M
//! pixels. Same renderer, same notation, different constant.
//!
//! So the constant is gone. [`plan`] derives the pacing from the schedule's own
//! **event density**, and the two clamps below are the pipeline's defence
//! against a caller who asks for something the notation cannot draw.
//!
//! # The two clamps, and where the numbers come from
//!
//! * [`MIN_STEP_MS`] — a frame that advances the world by less than one
//!   presentation frame is a duplicate. 40 ms is 25 fps, the GIF rate, so the
//!   floor is "never slower than real time".
//! * [`MAX_STEP_MS`] — PRD §11.4 puts the arrival pulse at ≤400 ms and this
//!   module's own [`crate::live::PULSE_SECS`] implements it. A frame that
//!   advances the world by more than a second steps straight over every pulse,
//!   and motion onset — *the* thing peripheral vision is good at — never
//!   happens. One world second is the ceiling.
//!
//! # Mass, not events
//!
//! "Change per frame" cannot be counted in records: a transcript's record count
//! is dominated by assistant text and thinking blocks, and the densest window by
//! record count in one real session held 13 676 records and **one** file
//! operation. So [`plan`] counts only what moves the live layer
//! ([`moves_the_map`]) and adds one deliberate distortion:
//!
//! A moment where a thread is **waiting on a human** (PRD §11.2's primary state)
//! carries no events at all — that is what waiting *is*. Scored on events alone
//! it is the emptiest part of the session and every window search walks straight
//! past it. So a [`Interest::Decision`] moment is worth
//! [`DECISION_DWELL_FRAMES`] frames of mass, which both pulls the window toward
//! it and makes the frame placement dwell there. PRD §17: *"does it change a
//! decision?"* — a recording that never shows the state the product exists for
//! cannot.
//!
//! # Frames are placed by mass, not by time
//!
//! Uniform time sampling gives a burst and its silence the same number of
//! frames. [`plan`] instead inverts the window's **cumulative mass curve**:
//! frame *i* lands where the mass first reaches `i / (frames - 1)` of the
//! total. A burst is a steep stretch of that curve and collects many frames;
//! dead air is flat and collects one; a decision is a jump, and the frames that
//! land on it are the dwell. A single forward repair pass then forces every
//! step inside the two clamps — which is the piece the M2 pipeline had no
//! equivalent of, and the piece that lets [`Pacing::verdict`] name a bad fit
//! instead of shipping it.

// Every ratio in this module is a count or a millisecond count divided by
// another, at magnitudes that fit in an `f64` mantissa a thousand times over: a
// session is millions of milliseconds and tens of thousands of events, not
// 2^53 of either. The same allow, for the same reason, as `plan` and `live`.
#![allow(clippy::cast_precision_loss)]

use polis_events::{Payload, RecordedEvent, TranscriptRecordKind};
use polis_world::replay::{Interest, ReplaySchedule};
use serde_json::Value;

/// The target amount of change in one frame, in live events.
///
/// Two is the smallest number that can read as *motion* rather than as a state
/// change: one event per frame is a slideshow of stills, and the eye needs the
/// second mark to know the first one moved.
pub const MASS_PER_FRAME: f64 = 2.0;

/// The least world time one frame may advance, in milliseconds.
///
/// 40 ms is one frame at the 25 fps the GIF encoder writes, so the floor says
/// "a recording is never slower than the session was".
pub const MIN_STEP_MS: u64 = 40;

/// The most world time one frame may advance, in milliseconds.
///
/// PRD §11.4's arrival pulse is ≤400 ms. A frame that jumps more than a world
/// second lands past every pulse it should have shown, and the replay reads as
/// a slideshow no matter how good the tween is.
pub const MAX_STEP_MS: u64 = 1_000;

/// How many frames a "waiting on you" moment is worth.
///
/// PRD §11.2 calls needs-decision *"the primary state; it is what the product is
/// for"*, and it is the one state that produces **no events while it lasts**.
/// Eight frames is a third of a second of playback at 25 fps — long enough to
/// read the pin, short enough that a session with forty prompts in it does not
/// become a recording of forty pins.
pub const DECISION_DWELL_FRAMES: f64 = 8.0;

/// How much quiet the recording opens on, in milliseconds.
///
/// Without it the first frame is the busiest instant of the session with an
/// empty map behind it, and the viewer sees the state arrive with no before.
const LEAD_IN_MS: u64 = 2_000;

/// A pacing, derived from a schedule.
#[derive(Debug, Clone)]
pub struct Pacing {
    /// Where the recording starts on the playback timeline, in milliseconds.
    pub start_ms: u64,
    /// Where it ends.
    pub end_ms: u64,
    /// One playback-timeline position per frame, ascending.
    pub frames_ms: Vec<u64>,
    /// The uniform step the density implies, before per-frame placement.
    /// Reported so a caller can see what the schedule asked for.
    pub step_ms: u64,
    /// Live events per second inside the window.
    pub events_per_second: f64,
    /// Live events inside the window.
    pub live_events: usize,
    /// Live events in the whole schedule.
    pub total_live_events: usize,
    /// "Waiting on you" moments inside the window.
    pub decisions: usize,
    /// Frames whose step hit [`MIN_STEP_MS`] — the burst was denser than the
    /// notation can draw and the recording is slower than the session.
    pub floored: usize,
    /// Frames whose step hit [`MAX_STEP_MS`] — dead air, crossed at the ceiling.
    pub ceilinged: usize,
}

impl Pacing {
    /// How many frames were placed.
    #[must_use]
    pub fn frames(&self) -> usize {
        self.frames_ms.len()
    }

    /// The playback position of frame `i`, saturating at the last one.
    #[must_use]
    pub fn frame_ms(&self, i: usize) -> u64 {
        self.frames_ms
            .get(i)
            .copied()
            .or_else(|| self.frames_ms.last().copied())
            .unwrap_or(0)
    }

    /// The mean world time one frame advances, in seconds — the number the M2
    /// post-mortem found was doing all the work.
    #[must_use]
    pub fn mean_step_secs(&self) -> f64 {
        let n = self.frames_ms.len();
        if n < 2 {
            return 0.0;
        }
        (self.frames_ms[n - 1].saturating_sub(self.frames_ms[0])) as f64 / (n - 1) as f64 / 1000.0
    }

    /// Whether the derived pacing landed inside the range the notation can
    /// draw, rather than against a clamp for most of the recording.
    ///
    /// An unclamped step is the mass curve getting what it asked for. Once more
    /// than half the steps had to be clamped at all, the frame budget and the
    /// schedule do not fit each other, and the **dominant** clamp names which
    /// way: mostly ceiling is a recording of silence, mostly floor is a burst
    /// being stretched over more frames than it has events for. Either is a
    /// caller problem the pipeline can now say out loud instead of shipping.
    #[must_use]
    pub fn verdict(&self) -> &'static str {
        let n = self.frames().max(1);
        if (self.floored + self.ceilinged) * 2 <= n {
            "paced by event density"
        } else if self.ceilinged >= self.floored {
            "mostly dead air: over half the frames hit the step ceiling"
        } else {
            "denser than the frame budget: over half the frames hit the step floor"
        }
    }
}

/// Whether an event can move the live layer.
///
/// Records that only add text to the transcript change no pixel, and a window
/// chosen by record count lands on the session's longest monologue. This is the
/// predicate that keeps that from happening.
#[must_use]
pub fn moves_the_map(event: &RecordedEvent) -> bool {
    match &event.event.payload {
        Payload::Transcript(t) => match t.kind {
            TranscriptRecordKind::Assistant => has_block(&t.record, "tool_use"),
            TranscriptRecordKind::User => has_block(&t.record, "tool_result"),
            // Journal lifecycle — a worker appearing or finishing is a tether
            // arriving or dimming — and attachments, because an `@`-mention is
            // the only record of a file with no tool call behind it (ADR-0017)
            // and it still places evidence, and therefore ink.
            TranscriptRecordKind::Started
            | TranscriptRecordKind::Result
            | TranscriptRecordKind::Attachment => true,
            _ => false,
        },
        Payload::Hook(_) | Payload::Otel(_) | Payload::Fs(_) => true,
        _ => false,
    }
}

/// Whether a transcript record carries a content block of the given type.
fn has_block(record: &Value, kind: &str) -> bool {
    record
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some(kind))
        })
}

/// One weighted moment on the playback timeline.
#[derive(Debug, Clone, Copy)]
struct Point {
    ms: u64,
    mass: f64,
    live: bool,
    decision: bool,
}

/// Derives a pacing for `frames` frames of `schedule`.
///
/// Returns a pacing with one frame when the schedule is empty, so a caller never
/// has to special-case it.
#[must_use]
pub fn plan(schedule: &ReplaySchedule, frames: usize) -> Pacing {
    let frames = frames.max(2);
    let decision_mass = DECISION_DWELL_FRAMES * MASS_PER_FRAME;
    let points: Vec<Point> = schedule
        .entries
        .iter()
        .filter_map(|e| {
            let live = moves_the_map(&e.event);
            let decision = e.interest == Some(Interest::Decision);
            if !live && !decision {
                return None;
            }
            Some(Point {
                ms: e.schedule_ms,
                mass: if live { 1.0 } else { 0.0 } + if decision { decision_mass } else { 0.0 },
                live,
                decision,
            })
        })
        .collect();

    let total_live = points.iter().filter(|p| p.live).count();
    if points.is_empty() {
        let end = schedule.duration_ms();
        let step = (end / (frames as u64 - 1)).clamp(MIN_STEP_MS, MAX_STEP_MS);
        return Pacing {
            start_ms: 0,
            end_ms: end,
            frames_ms: (0..frames).map(|i| (i as u64) * step).collect(),
            step_ms: step,
            events_per_second: 0.0,
            live_events: 0,
            total_live_events: 0,
            decisions: 0,
            floored: 0,
            ceilinged: frames - 1,
        };
    }

    // --- the uniform step the whole schedule implies ------------------------
    let span_ms = points
        .last()
        .map_or(0, |p| p.ms)
        .saturating_sub(points[0].ms)
        .max(1);
    let mass_total: f64 = points.iter().map(|p| p.mass).sum();
    let step_hint = derive_step(mass_total, span_ms);

    // --- the window: as much of the session as the frame budget can pace -----
    //
    // Twice, at most. A first plan that spends over half its frames against the
    // **floor** is one the frame budget cannot hold: the mass curve wanted a
    // shorter step than 40 ms and got 40, so those frames are crawling through
    // a burst rather than showing it. Covering more of the session is strictly
    // better than crawling, so the retry widens to the ceiling. Measured on
    // session `6f51089f`, whose ingest artefact puts 97% of the session's live
    // events into ten seconds, this is the difference between recording ten
    // seconds and recording a minute and a half.
    let mut hint = step_hint;
    let mut plan = window_and_walk(&points, hint, frames);
    if plan.3 * 2 > frames && hint < MAX_STEP_MS {
        hint = MAX_STEP_MS;
        plan = window_and_walk(&points, hint, frames);
    }
    let (start_ms, end_ms, inside, floored, ceilinged, frames_ms) = plan;
    let window_mass: f64 = inside.iter().map(|p| p.mass).sum();
    let window_ms = end_ms.saturating_sub(start_ms).max(1);
    let step_ms = derive_step(window_mass, window_ms);

    let end = frames_ms.last().copied().unwrap_or(end_ms);
    let live_events = inside.iter().filter(|p| p.live).count();
    Pacing {
        start_ms,
        end_ms: end,
        frames_ms,
        step_ms,
        events_per_second: live_events as f64 * 1000.0 / window_ms as f64,
        live_events,
        total_live_events: total_live,
        decisions: inside.iter().filter(|p| p.decision).count(),
        floored,
        ceilinged,
    }
}

/// One attempt: pick the window for `step_hint`, then place the frames in it.
///
/// Returns `(start, end, the points inside, floored, ceilinged, placements)`.
type Attempt = (u64, u64, Vec<Point>, usize, usize, Vec<u64>);

fn window_and_walk(points: &[Point], step_hint: u64, frames: usize) -> Attempt {
    let (start_ms, end_ms) = best_window(points, step_hint, frames);
    let inside: Vec<Point> = points
        .iter()
        .copied()
        .filter(|p| p.ms >= start_ms && p.ms <= end_ms)
        .collect();
    let mass: f64 = inside.iter().map(|p| p.mass).sum();
    let (frames_ms, floored, ceilinged) = walk(&inside, start_ms, end_ms, mass, frames);
    (start_ms, end_ms, inside, floored, ceilinged, frames_ms)
}

/// Places `frames` frames across `[start_ms, end_ms]`, one every
/// `mass / (frames - 1)`, then repairs the two clamps.
///
/// This is the inverse of the cumulative-mass curve: frame *i* lands where the
/// window's mass first reaches `i / (frames - 1)` of its total. A burst is a
/// steep stretch of that curve and collects many frames; dead air is flat and
/// collects one; an impulse — a "waiting on you" moment, worth
/// [`DECISION_DWELL_FRAMES`] frames — is a jump, and the frames that land on it
/// are what makes the recording dwell there.
///
/// The repair pass is what the M2 pipeline had no equivalent of. It walks
/// forward once and forces every step inside `[MIN_STEP_MS, MAX_STEP_MS]`, so
/// no frame can repeat its predecessor and none can step over PRD §11.4's
/// 400 ms arrival pulse. It returns how many steps hit each clamp, which is the
/// diagnosis [`Pacing::verdict`] reports rather than shipping.
fn walk(
    points: &[Point],
    start_ms: u64,
    end_ms: u64,
    mass: f64,
    frames: usize,
) -> (Vec<u64>, usize, usize) {
    let mut frames_ms = Vec::with_capacity(frames);
    let mut cum = 0.0;
    let mut idx = 0usize;
    for i in 0..frames {
        let target = mass * i as f64 / (frames as f64 - 1.0);
        while idx < points.len() && cum + points[idx].mass <= target {
            cum += points[idx].mass;
            idx += 1;
        }
        let at = points.get(idx).map_or(end_ms, |p| p.ms);
        frames_ms.push(at.clamp(start_ms, end_ms));
    }
    let mut floored = 0usize;
    let mut ceilinged = 0usize;
    for i in 1..frames_ms.len() {
        let prev = frames_ms[i - 1];
        let lo = prev.saturating_add(MIN_STEP_MS);
        let hi = prev.saturating_add(MAX_STEP_MS);
        // A step below the floor is a burst denser than the frame budget can
        // hold; one above the ceiling is dead air being crossed.
        if frames_ms[i] < lo {
            floored += 1;
        } else if frames_ms[i] > hi {
            ceilinged += 1;
        }
        frames_ms[i] = frames_ms[i].clamp(lo, hi);
    }
    (frames_ms, floored, ceilinged)
}

/// The step one frame should take to hold [`MASS_PER_FRAME`], clamped.
fn derive_step(mass: f64, span_ms: u64) -> u64 {
    if mass <= 0.0 {
        return MAX_STEP_MS;
    }
    let per_ms = mass / span_ms as f64;
    let raw = MASS_PER_FRAME / per_ms;
    if !raw.is_finite() || raw <= 0.0 {
        return MAX_STEP_MS;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms = raw.min(f64::from(u32::MAX)) as u64;
    ms.clamp(MIN_STEP_MS, MAX_STEP_MS)
}

/// The window worth recording, as `(start, end)`.
///
/// # Not the densest one, and not scored in events
///
/// The densest window is the wrong answer, and on real data it is spectacularly
/// wrong. `polis-ingest`'s offline full read walks a session's ninety transcript
/// files in file order with a running-maximum timestamp, so a fleet's subagent
/// records pile onto the last seconds of the schedule: measured on session
/// `6f51089f`, **97% of the whole session's live events land inside a ten-second
/// stretch at the very end**. Twenty thousand events arriving in one second are
/// one frame's worth of change, not twenty thousand, and a search that counts
/// events walks straight into the pile.
///
/// So a window is scored by **how many of its frames could carry change**: the
/// number of distinct [`MIN_STEP_MS`] buckets holding at least one live event,
/// which is the most frames that stretch can ever fill, plus
/// [`DECISION_DWELL_FRAMES`] for each "waiting on you" moment (PRD §17 — a
/// window with no decision in it cannot show one). It is capped at the frame
/// budget, because you cannot show more change than you have frames for, and
/// ties go to the window with more decisions, then to the one holding **fewer**
/// events, which is the one closest to the ideal pace and therefore the one
/// whose steps need the least clamping.
///
/// The width is `(frames - 1) × step_hint`; [`plan`] retries once at the
/// ceiling when the first attempt turns out to be floor-pinned.
///
/// Two-pointer over the already-sorted points, so the answer does not depend on
/// iteration order — the same determinism rule PRD §7.4 imposes on the layout,
/// applied here because this picks what the operator sees.
fn best_window(points: &[Point], step_hint: u64, frames: usize) -> (u64, u64) {
    let span = (frames as u64 - 1).max(1);
    let width = span.saturating_mul(step_hint);
    if points.is_empty() {
        return (0, width);
    }
    // You cannot show more change than you have frames for, so mass past the
    // budget is not worth anything and every window past the cap is equally
    // good. The tie-breaks are what then decide.
    //
    // Scored on **live** mass only. A decision's dwell mass belongs to the
    // frame placement, not to "does this stretch have enough happening in it":
    // counted here it let ten pins carry a window with forty-three tool calls
    // in it past the cap, and the recording became ninety seconds of an empty
    // map with a pin on it.
    let cap = (frames as f64 - 1.0) * MASS_PER_FRAME;
    // The search span is the window minus its lead-in, so that adding the
    // lead-in back does not shift the window off the mass it was chosen for —
    // which it did, and cost a whole session's recording: the shift dropped the
    // last two seconds of a window whose mass sat at its right edge, and the
    // planner picked a stretch with 46 live events in it.
    let search = width.saturating_sub(LEAD_IN_MS).max(1);
    let mut best = (f64::NEG_INFINITY, 0usize, f64::NEG_INFINITY, points[0].ms);
    let mut lo = 0usize;
    let mut live = 0.0_f64;
    let mut decisions = 0usize;
    for hi in 0..points.len() {
        if points[hi].live {
            live += points[hi].mass;
        }
        decisions += usize::from(points[hi].decision);
        while points[hi].ms.saturating_sub(points[lo].ms) > search {
            if points[lo].live {
                live -= points[lo].mass;
            }
            decisions -= usize::from(points[lo].decision);
            lo += 1;
        }
        // Maximise: useful mass, then decisions, then **least** live mass —
        // the window closest to the ideal pace, and therefore the one whose
        // steps need the least clamping. `total_cmp` throughout, so the ties
        // that decide this are exact rather than approximate.
        let score = (live.min(cap), decisions, -live);
        let better = score
            .0
            .total_cmp(&best.0)
            .then_with(|| score.1.cmp(&best.1))
            .then_with(|| score.2.total_cmp(&best.2))
            .is_gt();
        if better {
            best = (score.0, score.1, score.2, points[lo].ms);
        }
    }
    let start = best.3.saturating_sub(LEAD_IN_MS);
    (start, start.saturating_add(width))
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_events::{
        Channel, Event, EventMeta, RecordingHeader, SessionId, TranscriptSource, WallTime,
        RECORDING_FORMAT,
    };
    use polis_events::{Payload as P, TranscriptEvent};
    use polis_ingest::transcript::ParseStats;

    fn tool_use_record() -> Value {
        serde_json::json!({
            "message": { "content": [{ "type": "tool_use", "name": "Read", "id": "t" }] }
        })
    }

    fn text_record() -> Value {
        serde_json::json!({
            "message": { "content": [{ "type": "text", "text": "thinking out loud" }] }
        })
    }

    fn event(ms: u64, record: Value) -> RecordedEvent {
        let meta = EventMeta::now(Channel::Transcript).with_session(SessionId::new("s"));
        RecordedEvent {
            monotonic_offset_ms: ms,
            wall: WallTime::from_unix_millis(i64::try_from(ms).unwrap_or(0)),
            event: Event::new(
                meta,
                P::Transcript(Box::new(TranscriptEvent {
                    kind: TranscriptRecordKind::Assistant,
                    source: TranscriptSource::Main,
                    byte_offset: ms,
                    record,
                })),
            ),
        }
    }

    /// A hook `PermissionRequest`, which `replay::classify` labels
    /// [`Interest::Decision`] — the thing the dwell mass is for.
    fn decision(ms: u64) -> RecordedEvent {
        use polis_events::{EventKind, HookEvent, HookPayload};
        let meta = EventMeta::now(Channel::Hook).with_session(SessionId::new("s"));
        RecordedEvent {
            monotonic_offset_ms: ms,
            wall: WallTime::from_unix_millis(i64::try_from(ms).unwrap_or(0)),
            event: Event::new(
                meta,
                P::Hook(Box::new(HookEvent {
                    kind: EventKind::PermissionRequest,
                    truncated: false,
                    payload: HookPayload::default(),
                })),
            ),
        }
    }

    fn schedule(events: Vec<RecordedEvent>) -> ReplaySchedule {
        let header = RecordingHeader {
            format: RECORDING_FORMAT,
            wall_origin: WallTime::from_unix_millis(1_756_713_600_000),
            producer: "test".to_owned(),
        };
        ReplaySchedule::from_events(header, events, ParseStats::default(), Vec::new())
    }

    #[test]
    fn text_records_are_not_change() {
        // The densest window by record count in a real session held 13 676
        // records and one file operation.
        assert!(moves_the_map(&event(0, tool_use_record())));
        assert!(!moves_the_map(&event(0, text_record())));
    }

    #[test]
    fn a_dense_burst_is_paced_at_the_floor_and_dead_air_at_the_ceiling() {
        // 60 tool calls in 600 ms, then nothing for a minute.
        let mut events: Vec<RecordedEvent> =
            (0..60).map(|i| event(i * 10, tool_use_record())).collect();
        events.push(event(60_000, tool_use_record()));
        let p = plan(&schedule(events), 32);
        assert_eq!(p.frames(), 32);
        for pair in p.frames_ms.windows(2) {
            let step = pair[1] - pair[0];
            assert!(
                (MIN_STEP_MS..=MAX_STEP_MS).contains(&step),
                "every step is inside the clamps: {step}"
            );
        }
        assert!(
            p.frames_ms
                .windows(2)
                .any(|w| w[1] - w[0] <= MIN_STEP_MS + 10),
            "the burst is drawn at the floor"
        );
    }

    #[test]
    fn frames_never_go_backwards_and_never_stall() {
        let events: Vec<RecordedEvent> = (0..200)
            .map(|i| {
                event(
                    i * 137,
                    if i % 3 == 0 {
                        tool_use_record()
                    } else {
                        text_record()
                    },
                )
            })
            .collect();
        let p = plan(&schedule(events), 64);
        assert_eq!(p.frames(), 64);
        for pair in p.frames_ms.windows(2) {
            assert!(pair[1] >= pair[0] + MIN_STEP_MS, "a frame always advances");
        }
    }

    #[test]
    fn an_empty_schedule_still_produces_frames() {
        let p = plan(&schedule(Vec::new()), 16);
        assert_eq!(p.frames(), 16);
        assert_eq!(p.live_events, 0);
    }

    #[test]
    fn the_verdict_names_a_recording_of_dead_air() {
        // One call every thirty seconds for half an hour. Every frame wants to
        // jump 30 s and is held to 1 s, which is a recording of silence — and
        // the pipeline now says so instead of shipping it.
        let events: Vec<RecordedEvent> = (0..60)
            .map(|i| event(i * 30_000, tool_use_record()))
            .collect();
        let p = plan(&schedule(events), 48);
        assert!(
            p.ceilinged >= p.floored,
            "{} ceiling against {} floor",
            p.ceilinged,
            p.floored
        );
        assert_eq!(
            p.verdict(),
            "mostly dead air: over half the frames hit the step ceiling"
        );
    }

    #[test]
    fn a_waiting_on_you_moment_is_worth_frames_even_though_it_carries_no_events() {
        // PRD §11.2's primary state produces no events *while it lasts* — that
        // is what waiting is — so a window search scored on event count walks
        // straight past it. The dwell mass is what stops that: the pin's
        // instant collects DECISION_DWELL_FRAMES frames' worth of the
        // cumulative curve, and the recording lingers on it.
        let mut events: Vec<RecordedEvent> =
            (0..40).map(|i| event(i * 100, tool_use_record())).collect();
        events.push(decision(6_000));
        let p = plan(&schedule(events), 48);
        assert_eq!(p.decisions, 1, "the pin is inside the window");
        let pin = 6_000u64;
        let on_pin = p
            .frames_ms
            .iter()
            .filter(|ms| ms.abs_diff(pin) <= MIN_STEP_MS * 4)
            .count();
        assert!(
            on_pin >= 4,
            "the recording dwells on the pin: {on_pin} frames of {:?}",
            p.frames_ms
        );
    }

    #[test]
    fn the_step_is_derived_from_density_not_from_the_caller() {
        // Ten times the events in the same span is a tenth of the step, until
        // the floor.
        let sparse: Vec<RecordedEvent> =
            (0..20).map(|i| event(i * 500, tool_use_record())).collect();
        let dense: Vec<RecordedEvent> =
            (0..200).map(|i| event(i * 50, tool_use_record())).collect();
        let a = plan(&schedule(sparse), 64);
        let b = plan(&schedule(dense), 64);
        assert!(
            b.step_ms < a.step_ms,
            "the denser schedule gets the shorter step: {} vs {}",
            b.step_ms,
            a.step_ms
        );
    }
}
