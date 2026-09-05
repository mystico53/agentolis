//! Driving the world from a live feed, once per frame (PRD §5, §15 M3).
//!
//! > **Live, single session.** Wire M0 into M2. One agent, real time. (PRD §15)
//!
//! Every ingest channel worked and every renderer worked, and there was no code
//! path between them: `polis-app`'s window never imported `polis-ingest`, and
//! the one function that held an `EventSource` had *"keeps the bus empty"* in
//! its own doc comment. So an operator ran a real agent for minutes and the map
//! showed nothing. This module is that missing path — the ingest stack, the
//! per-frame drain, and the two panels that make sure the window is never
//! silent again.
//!
//! [`crate::status::Connectivity`] is the other half and lives elsewhere on
//! purpose: `polis watch --list` and `polis doctor` have to answer "what is
//! connected" having bound nothing, and this module binds two ports.
//!
//! # Where the world runs, and why
//!
//! On the **main thread**, drained once per frame, exactly like a replay.
//! [`crate::app`]'s module docs called moving the publisher to a world thread
//! "a two-line change when live ingest lands"; it is, and it is not the right
//! change. Three numbers decide it, measured on this machine:
//!
//! * A whole live frame — apply, tick, publish — costs **0.12 ms** at PRD
//!   §13.1's 500 events/sec, and 0.29 ms at its worst. Measured by replaying
//!   this machine's own sessions in eight-event batches, which is what 500/sec
//!   is at 60 fps: 30 785 events over a 180-building city gave 7.8 µs an event,
//!   0.063 ms of apply and 0.058 ms of publish per frame; 14 979 events over a
//!   **1 592**-building one gave 6.3 µs, 0.050 ms and 0.039 ms. That is 0.7% of
//!   a 16.6 ms frame, and it does not grow with the city.
//! * A publish is rate-limited by
//!   [`polis_world::snapshot::MIN_PUBLISH_INTERVAL`], so the ingest rate cannot
//!   reach the renderer however fast events arrive: PRD §5's *"decouple event
//!   rate from frame rate"* is already enforced by the type on the other side.
//! * A world thread would have to send the layout back for `set_layout`, and
//!   would put a channel between the click that selects a building and the world
//!   that knows about it.
//!
//! The one case that does not fit a frame is a **burst**, and a thread would not
//! fix it either — it would only move it. Channel D backfills up to
//! [`polis_ingest::live::BACKFILL_BYTES`] per session when a watch opens, and on
//! this machine that is 18 000 events in the first second; the whole 30 785-event
//! session applied in one go is 234 ms. So the burst is spread across frames by
//! [`APPLY_BUDGET`] instead, and the leftovers ask for the next frame.
//!
//! So the world stays where the replay's world already is, and the one thing a
//! thread *is* needed for is the one thing the main thread genuinely cannot do:
//! notice that an event arrived while `winit` is asleep. That is the
//! `polis-live-wake` thread, and it never touches the world.
//!
//! # The idle budget is why there is a thread at all
//!
//! > Idle CPU (no agent activity): < 2% of one core. (PRD §13.1)
//!
//! `egui` draws on input and on request. A live window that polls the bus with
//! `request_repaint_after` pays a **whole rendered frame** per poll: a 100 ms
//! poll is 10 fps of a city forever, and a 1 s poll makes the first event of a
//! new agent land a second late. A `polis-live-wake` thread instead polls the
//! bus's queue length — a handful of relaxed atomic loads — every [`WAKE_POLL`],
//! and asks for a frame only when there is something to draw. The window
//! therefore renders **because an event arrived**, and an idle Polis costs one
//! sleeping thread.
//!
//! It is handed a closure rather than the [`EventSource`], so the thread that
//! watches the bus cannot consume from it. PRD §5's single consumer is a
//! property of what was handed over, not of a comment.
//!
//! # Scope: which agents belong on this map
//!
//! `polis-ingest` scopes **Channel D** to this checkout
//! ([`polis_ingest::SessionScope`]), which is what makes zero-configuration
//! presence work. It cannot scope Channels A and B: hooks registered in
//! `~/.claude/settings.json` fire for every repository on the machine, and a
//! telemetry block exported into a shell follows that shell everywhere. Without
//! a second gate, an agent working in a *different* checkout has its
//! `src/main.rs` land on *this* city's `src/main.rs` — and two unrelated agents
//! render as a contention over one building, which is the most decision-changing
//! mark the map has.
//!
//! `ScopeFilter` is that gate. It decides per session, from the `cwd` every
//! hook payload carries (`docs/verified/hooks-schema.md` §2) and every threaded
//! transcript record carries (`docs/verified/jsonl-schema.md` §3: `cwd`, STABLE
//! 100%, absolute). Events that arrive before the verdict is known are **held**,
//! in order, and released the instant it is; after [`SCOPE_GRACE`] with no
//! evidence at all they are let through and counted, because a silent empty map
//! is the failure this milestone exists to fix and a foreign thread in the
//! status rail is a visible, countable mistake where a missing one is invisible.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use eframe::egui::{self, RichText};
use polis_events::{Channel, Event, PathMapper, Payload, SessionId};
use polis_ingest::{EventSource, Ingest, IngestConfig, SourceHealth};
use polis_world::snapshot::WorldSnapshot;
use polis_world::{ThreadStatus, World};

use crate::palette;
use crate::status::{Connectivity, Reach};

/// How often the `polis-live-wake` thread looks at the bus's queue length.
///
/// One frame. Long enough that the thread is asleep essentially all the time,
/// short enough that PRD §11.4's *"the arrival of a mark is a brief pulse"*
/// starts on the frame after the event rather than a poll interval later.
pub const WAKE_POLL: Duration = Duration::from_millis(16);

/// How long one frame may spend applying events.
///
/// PRD §13.1's steady-state frame budget is 16.6 ms and the map still has to be
/// drawn inside it. A batch bigger than this is not dropped — it is carried, and
/// the carry is what asks for the next frame. A storm therefore costs frames,
/// never frame rate.
pub const APPLY_BUDGET: Duration = Duration::from_millis(6);

/// The most events one frame will take off the bus, whatever the clock says.
///
/// A ceiling as well as a time budget, because [`Instant::now`] is not free
/// enough to call per event: the loop consults the clock every
/// `BUDGET_CHECK_EVERY` events, and this bounds the tail between two checks.
pub const MAX_PER_PUMP: usize = 4_096;

/// How often the apply loop consults the clock.
const BUDGET_CHECK_EVERY: usize = 64;

/// How long an event waits for its session's `cwd` before it is let through
/// anyway.
///
/// Far longer than the gap between an agent's first telemetry and its first
/// transcript record, and far shorter than an operator's patience with a map
/// that shows nothing.
pub const SCOPE_GRACE: Duration = Duration::from_secs(2);

/// How many events may be held awaiting a verdict before the hold is flushed.
pub const SCOPE_HOLD_CAP: usize = 4_096;

/// The heartbeat while an agent is alive but quiet.
///
/// A thread goes `Idle` after inactivity and a trail fades, and neither happens
/// without a [`World::tick`]. Two frames a second ages the world and is two
/// orders of magnitude below a repaint loop.
pub const LIVE_TICK: Duration = Duration::from_millis(500);

/// The heartbeat with no agent in sight.
///
/// Only the "waiting for an agent — 42 s" clock needs this, so it is as slow as
/// a clock that counts seconds can be.
pub const IDLE_TICK: Duration = Duration::from_secs(1);

/// How often the reporter thread asks whether the connectivity report could
/// have changed.
///
/// The *asking* is a handful of counters and a roster clone. The rebuilding is
/// **86 ms** — [`Connectivity::of`] answers "are hooks registered" through
/// `setup::detect`, which indexes every session on the machine (203 of them
/// here) to answer a question about two settings files. So the report is
/// rebuilt when a cheap fingerprint of it moves, not on a timer: measured, it
/// moves a handful of times in a session, and polling it at this interval would
/// cost 8.6% of a core for ever.
pub const REPORT_POLL: Duration = Duration::from_millis(1_000);

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Which agents a live window draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// Only sessions working inside this checkout or one of its worktrees.
    ///
    /// The default, and what *"see every agent active in a repository"* means: a
    /// window titled with one repository must not grow buildings out of another
    /// one's edits.
    #[default]
    Repo,
    /// Every session on the machine, wherever it is working.
    ///
    /// Honest only when the operator asked for it: paths outside this checkout
    /// have no building, so foreign threads land in the status rail unplaced.
    Machine,
}

/// Everything [`crate::app::Mode::Live`] needs to start.
///
/// Carries an [`IngestConfig`] rather than re-deriving one, so the caller
/// decides which channels run and which sessions Channel D follows.
#[derive(Debug, Clone)]
pub struct LiveOptions {
    /// How to start the four channels. `repo_root` is also the city.
    pub ingest: IngestConfig,
    /// Which agents belong on this map, once the events are on the bus.
    pub scope: Scope,
}

impl LiveOptions {
    /// Every default, for a checkout.
    pub fn for_repo(repo: impl Into<PathBuf>) -> Self {
        Self {
            ingest: IngestConfig::new(repo),
            scope: Scope::Repo,
        }
    }

    /// The checkout this window is for.
    pub fn repo(&self) -> &Path {
        &self.ingest.repo_root
    }
}

// ---------------------------------------------------------------------------
// The feed
// ---------------------------------------------------------------------------

/// A running ingest stack and the world it drives.
///
/// Dropping it shuts every channel down and stops the waker.
#[derive(Debug)]
pub struct LiveFeed {
    /// Shared only so the waker thread can read the queue depth. This struct,
    /// on the main thread, is the single consumer PRD §5 requires.
    source: Arc<EventSource>,
    /// Behind a mutex because the reporter thread reads counters from it too.
    /// `Option` so shutdown can take it out and consume it.
    ingest: Arc<Mutex<Option<Ingest>>>,
    waker: Option<Helper>,
    reporter: Option<Helper>,
    scope: ScopeFilter,
    repo: PathBuf,
    /// Published by the reporter thread, sampled here — the same lock-free
    /// shape PRD §5 already requires of the world snapshot, for the same
    /// reason: the expensive side must never be on the frame's critical path.
    report_cell: Arc<ArcSwap<Connectivity>>,
    report: Arc<Connectivity>,
    /// Drained off the bus and not yet applied, oldest first.
    carry: VecDeque<Event>,
    /// Reused between frames, so a steady-state frame allocates nothing.
    scratch: Vec<Event>,
    ready: Vec<Event>,
    started: Instant,
    last_event: Option<Instant>,
    applied: u64,
    /// Events from agents working in another checkout.
    filtered: u64,
    /// Bus depth plus carry, at the last pump.
    queued: usize,
    dropped: u64,
}

/// What one [`LiveFeed::pump`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pumped {
    /// Events handed to [`World::apply`].
    pub applied: usize,
    /// Events discarded as another checkout's.
    pub filtered: usize,
    /// Events still waiting for their session's verdict.
    pub held: usize,
    /// Events taken off the bus and not yet applied, because the frame ran out
    /// of budget. Non-zero means the next frame is wanted immediately.
    pub backlog: usize,
}

impl LiveFeed {
    /// Starts every enabled channel and returns the feed.
    ///
    /// Call this **before** the city is generated. A 5 000-file cold start is
    /// seconds; a hook datagram that arrives with nothing bound is gone for
    /// good, and an OTel exporter that finds nothing listening drops its first
    /// batch — which is the session start. The bounded bus holds what arrives in
    /// the meantime.
    ///
    /// No [`egui::Context`] yet, deliberately: the ports have to be bound before
    /// the window exists. [`LiveFeed::wake_with`] attaches the waker once there
    /// is a context to wake.
    pub fn start(options: &LiveOptions) -> Self {
        let repo = options.ingest.repo_root.clone();
        let mapper = PathMapper::new(&repo).unwrap_or_default();
        let (ingest, source) = Ingest::start(options.ingest.clone(), mapper.clone());
        // The first report costs 86 ms and is paid here, inside PRD §13.1's
        // three-second cold start, so the window's very first frame already
        // says what is connected. Every later one is the reporter thread's.
        let report = Arc::new(Connectivity::of(&repo, &ingest));
        Self {
            source: Arc::new(source),
            ingest: Arc::new(Mutex::new(Some(ingest))),
            waker: None,
            reporter: None,
            scope: ScopeFilter::new(mapper, options.scope),
            repo,
            report_cell: Arc::new(ArcSwap::from(Arc::clone(&report))),
            report,
            carry: VecDeque::new(),
            scratch: Vec::new(),
            ready: Vec::new(),
            started: Instant::now(),
            last_event: None,
            applied: 0,
            filtered: 0,
            queued: 0,
            dropped: 0,
        }
    }

    /// Starts the two threads that wake the window: one when an event arrives,
    /// one when what is connected changes.
    ///
    /// Idempotent, so a second call cannot leak a thread.
    pub fn wake_with(&mut self, ctx: &egui::Context) {
        if self.waker.is_some() {
            return;
        }
        let source = Arc::clone(&self.source);
        let wake = ctx.clone();
        self.waker = Some(Helper::spawn("polis-live-wake", WAKE_POLL, move || {
            if source.stats().queued > 0 {
                wake.request_repaint();
            }
        }));

        let ingest = Arc::clone(&self.ingest);
        let cell = Arc::clone(&self.report_cell);
        let repo = self.repo.clone();
        let wake = ctx.clone();
        let mut last = Fingerprint::of(&ingest);
        self.reporter = Some(Helper::spawn("polis-live-report", REPORT_POLL, move || {
            let now = Fingerprint::of(&ingest);
            if now == last {
                return;
            }
            last = now;
            let Ok(guard) = ingest.lock() else {
                return;
            };
            let Some(ingest) = guard.as_ref() else {
                return;
            };
            cell.store(Arc::new(Connectivity::of(&repo, ingest)));
            drop(guard);
            wake.request_repaint();
        }));
    }

    /// Adopts the world's path mapper, which knows this repository's registered
    /// worktrees (PRD §7.6).
    ///
    /// The feed binds its ports before the city exists and therefore before
    /// `git worktree list` has been read, so its first mapper holds the primary
    /// checkout and nothing else. Called once, when the scene is built — without
    /// it, an agent working in a worktree of this repository is filtered out as
    /// a foreigner.
    pub fn adopt_mapper(&mut self, mapper: &PathMapper) {
        self.scope.mapper = mapper.clone();
    }

    /// Drains the bus, applies what is in scope, and ages the world.
    ///
    /// **Once per frame, never per event** (PRD §5). Returns what it did, so the
    /// caller can decide whether the next frame is wanted immediately.
    pub fn pump(&mut self, world: &mut World, now: Instant) -> Pumped {
        self.ready.clear();
        // Before the drain, so whatever the grace period releases is applied in
        // front of the events that arrived after it.
        self.scope.expire(now, &mut self.ready);

        if self.carry.is_empty() {
            self.scratch.clear();
            self.source.drain(&mut self.scratch);
            self.carry.extend(self.scratch.drain(..));
        }

        let deadline = Instant::now() + APPLY_BUDGET;
        let mut seen = 0usize;
        let mut filtered = 0usize;
        // Whatever the grace period released is already waiting.
        let mut applied = flush(&mut self.ready, world);
        let mut checked = 0usize;
        while let Some(event) = self.carry.pop_front() {
            seen += 1;
            if self.scope.admit(event, &mut self.ready) == Verdict::Drop {
                filtered += 1;
            }
            // Applied here, inside the budget, and not in one pass afterwards:
            // the budget has to bound the expensive half. It did not, once —
            // classification is nanoseconds and `World::apply` is microseconds,
            // so a frame that drained 4 096 events took 314 ms with a 6 ms
            // budget that had already been honoured.
            applied += flush(&mut self.ready, world);
            // The clock every few dozen events, not every event: `Instant::now`
            // costs about what applying one event costs. `seen` moves on a
            // filtered event and `applied` moves when a held batch is released,
            // so either can be what runs the budget out.
            if seen.is_multiple_of(BUDGET_CHECK_EVERY)
                || applied.saturating_sub(checked) >= BUDGET_CHECK_EVERY
            {
                checked = applied;
                if Instant::now() >= deadline {
                    break;
                }
            }
            if seen >= MAX_PER_PUMP {
                break;
            }
        }
        if applied > 0 {
            self.applied += applied as u64;
            self.last_event = Some(now);
        }
        // Once per batch, so territory half-life and claim expiry do not depend
        // on how chatty the fleet is.
        world.tick(now);

        self.filtered += filtered as u64;
        // `try_lock`, never `lock`: the reporter holds this for the 86 ms it
        // takes to rebuild the report, and a queue depth that is 86 ms stale is
        // invisible where a stalled frame is not.
        if let Ok(guard) = self.ingest.try_lock() {
            if let Some(ingest) = guard.as_ref() {
                let stats = ingest.stats();
                self.queued = stats.queued + self.carry.len();
                self.dropped = stats.dropped_total();
            }
        }
        self.report = self.report_cell.load_full();
        Pumped {
            applied,
            filtered,
            held: self.scope.held.len(),
            backlog: self.carry.len(),
        }
    }

    /// How long the window may sleep before the next frame.
    ///
    /// `ZERO` means "as soon as you can": there is a backlog. Otherwise it is
    /// the slowest heartbeat that still ages the world — [`LIVE_TICK`] while an
    /// agent is alive, [`IDLE_TICK`] when none is.
    pub fn wake_after(&self, snapshot: &WorldSnapshot, backlog: usize) -> Duration {
        if backlog > 0 {
            return Duration::ZERO;
        }
        if snapshot.threads.iter().any(|t| t.status.is_live()) {
            LIVE_TICK
        } else {
            IDLE_TICK
        }
    }

    /// What is and is not connected, as of the last refresh.
    pub fn report(&self) -> &Connectivity {
        &self.report
    }

    /// The checkout being watched.
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// Events applied to the world since the window opened.
    pub fn applied(&self) -> u64 {
        self.applied
    }

    /// Events discarded as belonging to another checkout.
    pub fn filtered(&self) -> u64 {
        self.filtered
    }

    /// Events dropped by the bounded bus (PRD §4.5).
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Events on the bus and in the carry, at the last pump.
    pub fn queued(&self) -> usize {
        self.queued
    }

    /// How long since an in-scope event was applied, or since the window opened
    /// when none ever has.
    pub fn quiet_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_event.unwrap_or(self.started))
    }

    /// Whether anything at all has been applied.
    pub fn has_seen_an_event(&self) -> bool {
        self.last_event.is_some()
    }

    /// Stops every channel and joins both helper threads.
    pub fn shutdown(&mut self) {
        // The threads first: the reporter takes the ingest lock, and shutting
        // the stack down underneath it would be a race worth not having.
        for helper in [self.waker.as_mut(), self.reporter.as_mut()]
            .into_iter()
            .flatten()
        {
            helper.stop();
        }
        self.waker = None;
        self.reporter = None;
        let taken = match self.ingest.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(ingest) = taken {
            ingest.shutdown();
        }
    }
}

impl Drop for LiveFeed {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// The two threads live mode adds
// ---------------------------------------------------------------------------

/// A background thread that looks at something on an interval and, when it is
/// worth a frame, asks for one.
///
/// Two of these exist and neither holds a world, a snapshot or an
/// [`EventSource`]. They are handed closures rather than the feed, so the
/// threads that watch the bus **cannot consume from it**: PRD §5's single
/// consumer is a property of what was handed over, not of a comment.
///
/// * `polis-live-wake`, every [`WAKE_POLL`] — reads the bus's queue length and
///   requests a repaint when it is non-empty. This is what makes the window
///   render *because an event arrived* rather than on a timer.
/// * `polis-live-report`, every [`REPORT_POLL`] — rebuilds
///   [`Connectivity`] when a cheap [`Fingerprint`] of it has moved. It is a
///   thread rather than a per-frame call because the rebuild is 86 ms, and a
///   real watch drew an 84 ms p99 doing it inline once a second.
#[derive(Debug)]
struct Helper {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Helper {
    fn spawn(name: &str, interval: Duration, mut body: impl FnMut() + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    body();
                    // `park_timeout` rather than `sleep`, so shutdown is
                    // immediate instead of up to one interval away.
                    std::thread::park_timeout(interval);
                }
            })
            .ok();
        Self { stop, handle }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The cheap summary of everything [`Connectivity::of`] branches on.
///
/// Computing this is a counter read and a roster clone — tens of microseconds.
/// Rebuilding the report it stands for is 86 ms, because `setup::detect`
/// indexes every session on the machine to answer a question about two settings
/// files. So the reporter compares fingerprints and rebuilds on a change, which
/// on real sessions happens a handful of times: a channel receives its first
/// event, an agent starts, an agent stops.
#[derive(Debug, Default, PartialEq, Eq)]
struct Fingerprint {
    health: Vec<(Channel, SourceHealth)>,
    /// Whether each channel has received anything at all — the boundary every
    /// `Reach` in the report turns on.
    arrived: [bool; 4],
    /// The roster's own one-line summary, which the strip shows verbatim.
    headline: Option<String>,
    tailing: usize,
    elsewhere: usize,
}

impl Fingerprint {
    fn of(ingest: &Mutex<Option<Ingest>>) -> Self {
        let Ok(guard) = ingest.lock() else {
            return Self::default();
        };
        let Some(ingest) = guard.as_ref() else {
            return Self::default();
        };
        let totals = ingest.totals();
        let roster = ingest.roster();
        Self {
            health: ingest.health(),
            arrived: [
                Channel::Otel,
                Channel::Hook,
                Channel::Fs,
                Channel::Transcript,
            ]
            .map(|c| totals.received(c) > 0),
            headline: roster.as_ref().map(polis_ingest::Roster::headline),
            tailing: roster.as_ref().map_or(0, polis_ingest::Roster::tailing),
            elsewhere: roster
                .as_ref()
                .map_or(0, polis_ingest::Roster::live_elsewhere),
        }
    }
}

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

/// What happened to one event on its way to the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Queued for [`World::apply`].
    Apply,
    /// Another checkout's. Counted, not drawn.
    Drop,
    /// Waiting for its session's `cwd`.
    Held,
}

/// Decides, per session, whether an event belongs on this map.
#[derive(Debug)]
struct ScopeFilter {
    mapper: PathMapper,
    scope: Scope,
    /// `true` = this checkout. Learned once per session and never revisited: a
    /// session's `cwd` moves inside a repository, not between repositories.
    verdict: BTreeMap<SessionId, bool>,
    /// Events whose session has no verdict yet, oldest first.
    held: VecDeque<Event>,
    /// When the oldest held event was observed.
    held_since: Option<Instant>,
    /// Events let through with no evidence either way.
    unscoped: u64,
}

impl ScopeFilter {
    fn new(mapper: PathMapper, scope: Scope) -> Self {
        Self {
            mapper,
            scope,
            verdict: BTreeMap::new(),
            held: VecDeque::new(),
            held_since: None,
            unscoped: 0,
        }
    }

    /// Classifies one event, appending everything now cleared to `ready`.
    ///
    /// Held events for a session are released **in front of** the event that
    /// taught the filter about it, so the world never sees a session's events
    /// out of order.
    fn admit(&mut self, event: Event, ready: &mut Vec<Event>) -> Verdict {
        if self.scope == Scope::Machine {
            ready.push(event);
            return Verdict::Apply;
        }
        // Channel C is rooted at this checkout by the watcher itself, and the
        // control channel is Polis talking about Polis. Neither has a session,
        // and a `ChannelDegraded` that never reached the world would be a status
        // bar that lies.
        if matches!(event.meta.channel, Channel::Fs | Channel::Control) {
            ready.push(event);
            return Verdict::Apply;
        }
        let Some(session) = session_of(&event).cloned() else {
            ready.push(event);
            return Verdict::Apply;
        };
        if let (Some(cwd), false) = (cwd_of(&event), self.verdict.contains_key(&session)) {
            let inside = self.mapper.to_logical_str(cwd).is_some();
            self.verdict.insert(session.clone(), inside);
            if inside {
                self.release(&session, ready);
            } else {
                self.discard(&session);
            }
        }
        match self.verdict.get(&session) {
            Some(true) => {
                ready.push(event);
                Verdict::Apply
            }
            Some(false) => Verdict::Drop,
            None => {
                if self.held.is_empty() {
                    self.held_since = Some(event.meta.observed);
                }
                self.held.push_back(event);
                Verdict::Held
            }
        }
    }

    /// Lets held events through once no verdict is coming.
    ///
    /// Fail-open, on purpose. The failure this milestone exists to fix is a map
    /// that showed nothing while a real agent worked, and a foreign thread in
    /// the status rail is a visible, countable mistake where a missing one is
    /// invisible.
    fn expire(&mut self, now: Instant, ready: &mut Vec<Event>) {
        let stale = self
            .held_since
            .is_some_and(|since| now.saturating_duration_since(since) >= SCOPE_GRACE);
        if self.held.is_empty() || !(stale || self.held.len() >= SCOPE_HOLD_CAP) {
            return;
        }
        for event in self.held.drain(..) {
            if let Some(session) = session_of(&event) {
                // Remembered, so the rest of the session flows straight through
                // instead of being held and expired one grace period at a time.
                self.verdict.insert(session.clone(), true);
            }
            self.unscoped += 1;
            ready.push(event);
        }
        self.held_since = None;
    }

    fn release(&mut self, session: &SessionId, ready: &mut Vec<Event>) {
        let mut keep = VecDeque::with_capacity(self.held.len());
        for event in self.held.drain(..) {
            if session_of(&event) == Some(session) {
                ready.push(event);
            } else {
                keep.push_back(event);
            }
        }
        self.held = keep;
        self.held_since = self.held.front().map(|e| e.meta.observed);
    }

    fn discard(&mut self, session: &SessionId) {
        self.held.retain(|event| session_of(event) != Some(session));
        self.held_since = self.held.front().map(|e| e.meta.observed);
    }
}

/// Applies a batch to the world and empties it, returning how many.
///
/// A free function so it can take one field of the feed while the loop holds
/// another.
fn flush(ready: &mut Vec<Event>, world: &mut World) -> usize {
    let n = ready.len();
    for event in ready.drain(..) {
        world.apply(&event);
    }
    n
}

/// The session an event belongs to.
///
/// `meta.session` first, because normalization fills it for every channel that
/// has one; the hook payload is the fallback for a datagram whose envelope did
/// not carry it.
fn session_of(event: &Event) -> Option<&SessionId> {
    if let Some(session) = event.meta.session.as_ref() {
        return Some(session);
    }
    match &event.payload {
        Payload::Hook(hook) => hook.payload.session_id.as_ref(),
        _ => None,
    }
}

/// The working directory an event reports, when it reports one.
///
/// Two channels carry it: every hook payload
/// (`docs/verified/hooks-schema.md` §2) and every threaded transcript record
/// (`docs/verified/jsonl-schema.md` §3 — `cwd`, STABLE 100%, absolute). The OTel
/// channel does not, which is why a verdict is a *session* property rather than
/// an event one.
fn cwd_of(event: &Event) -> Option<&str> {
    match &event.payload {
        Payload::Hook(hook) => hook.payload.cwd.as_deref(),
        Payload::Transcript(record) => record.record.get("cwd").and_then(|v| v.as_str()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// What the window says while it waits
// ---------------------------------------------------------------------------

/// The live strip, drawn immediately above the status bar.
///
/// Answers, in one line and always: which sources are reaching Polis, how many
/// agents are visible, how much has arrived, and how much the bounded bus
/// dropped — PRD §4.5 requires that counter surfaced, as a number even when it
/// is zero, because "nothing was dropped" and "the counter is not wired up" must
/// never look the same.
pub fn strip(ui: &mut egui::Ui, feed: &LiveFeed, snapshot: &WorldSnapshot, now: Instant) {
    let report = feed.report();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("LIVE")
                .monospace()
                .strong()
                .color(palette::selection().color()),
        );
        for line in &report.lines {
            ui.label(
                RichText::new(line.name)
                    .monospace()
                    .color(reach_colour(line.reach)),
            )
            .on_hover_text(match &line.hint {
                Some(hint) => format!("{}\n\n{hint}", line.detail),
                None => line.detail.clone(),
            });
        }
        ui.separator();

        // The headline is the roster's when there is one — it counts sessions on
        // disk, so it is right in the first second, before a single event has
        // arrived — and the world's own counts once events are landing.
        //
        // # Why this is not `snapshot.threads.len()` any more
        //
        // It used to read `{threads} on the map · {live} live`, and both halves
        // were wrong at once.
        //
        // *On the map* counted every thread in the world, including the ones the
        // status rail is explicitly drawing **because they are not on the map** —
        // PRD §6.2's *"the thread renders with no cloud — an unplaced marker in
        // the status rail"*. Watching `qurio-toolset` with eight real sessions
        // the strip said "7 on the map" while four of those seven rows read
        // `unplaced`. Two panels, one frame, two answers.
        //
        // *Live* was [`polis_world::ThreadStatus::is_live`], which is "not
        // done"; nothing in a transcript ends a session, so without hooks
        // registered no thread is ever done and the number was arithmetically
        // equal to the first one. Printing the same integer twice under two
        // names is PRD §17's test failed twice — it cannot change a decision,
        // and it makes the strip look like it is disagreeing with itself.
        //
        // The three numbers here are three different numbers, and the first two
        // sum to the rail's own row count *by construction*, because both
        // panels ask [`polis_world::territory::Territory::placement`].
        //
        // # Which question this asks, and which one the status bar asks
        //
        // `placed` is `placement().is_somewhere()` and nothing else. The status
        // bar's cloud census counts the same threads through
        // [`polis_world::territory::select_clouds`], which applies **two more
        // gates** on top of placement: a territory with no kernels (placed by
        // its evidence, but working entirely outside this checkout, so there is
        // nothing to splat) and a territory quiet past PRD §10.4's dormancy
        // window. Both land in the census's `UNCONVERGED` and `DORMANT`
        // buckets. So `placed` here is an upper bound on `shown + capped` there,
        // and the difference is not a disagreement: this line answers *"does
        // Polis know where this thread is"* and that one answers *"is there a
        // cloud on the map for it"*. Deliberately not sourced from the same
        // `CloudSelection`, because the rail's `unplaced` rows are keyed on
        // `placement()` and this line is what makes the two sum.
        let drawn = snapshot.threads.len();
        let placed = snapshot
            .threads
            .iter()
            .filter(|t| t.territory.placement().is_somewhere())
            .count();
        let headline = match (&report.roster, drawn) {
            (_, n) if n > 0 => {
                let working = snapshot
                    .threads
                    .iter()
                    .filter(|t| t.status == ThreadStatus::Working)
                    .count();
                format!(
                    "{placed} on the map · {} unplaced · {working} working",
                    n - placed
                )
            }
            (Some(roster), _) => roster.headline(),
            (None, _) => "waiting for an agent…".to_owned(),
        };
        ui.label(RichText::new(headline).monospace().color(if drawn == 0 {
            palette::needs_decision().color()
        } else {
            palette::worker().color()
        }))
        .on_hover_text(
            "On the map: threads whose evidence names a place — PRD §6.2's converged district or \
             §6.4's lobes — so they have a territory to draw. Two further gates then decide how \
             many of those get a cloud this frame: a territory with nothing inside this checkout \
             has no kernels to splat, one quiet past PRD §10.4's dormancy window dissipates, and \
             the cap takes the rest. The status bar's cloud line names whichever of those fired. \
             Unplaced: the rest, which is exactly the set the rail marks `unplaced`; the two \
             always sum to the rail's row count. Working: threads making tool calls, as opposed \
             to waiting on you or idle.",
        );

        ui.label(dim(format!(
            "{} events · last {}",
            feed.applied(),
            if feed.has_seen_an_event() {
                crate::format::duration(feed.quiet_for(now))
            } else {
                "never".to_owned()
            }
        )));

        let dropped = feed.dropped();
        ui.label(
            RichText::new(format!("dropped {dropped}"))
                .monospace()
                .color(if dropped == 0 {
                    palette::status(ThreadStatus::Idle).color()
                } else {
                    palette::contention().color()
                }),
        )
        .on_hover_text(
            "Events the bounded bus evicted rather than let backpressure reach an \
             agent (PRD §4.5). It drops the oldest and counts it; the number never \
             reaching an agent is the point.",
        );
        if feed.queued() > 0 {
            ui.label(dim(format!("queued {}", feed.queued())));
        }
        if feed.filtered() > 0 {
            ui.label(dim(format!("elsewhere {}", feed.filtered())))
                .on_hover_text(
                    "Events from agents working in another checkout. Their files have \
                     no building here, so they are counted rather than drawn onto the \
                     wrong one.",
                );
        }
    });
}

/// The height [`waiting`] needs, so the panel is sized to its content.
///
/// `egui::Panel` does not shrink to fit, and the first version asked for a flat
/// 76 px: on a real machine that cut the `hooks` row in half and dropped the
/// `files` row entirely — a panel whose whole job is to end silence, silently
/// truncated. The rows are known here, so the height is computed from them, and
/// the content scrolls if a narrow window wraps one anyway.
pub fn waiting_height(feed: &LiveFeed) -> f32 {
    let report = feed.report();
    let rows = report.lines.len() + usize::from(elsewhere_here(report) > 0);
    HEADLINE_HEIGHT + rows as f32 * ROW_HEIGHT + WAITING_PAD
}

/// The headline row of [`waiting`].
const HEADLINE_HEIGHT: f32 = 24.0;
/// One connectivity row.
const ROW_HEIGHT: f32 = 20.0;
/// Padding above and below.
const WAITING_PAD: f32 = 14.0;

/// Live sessions the roster can see in some other checkout.
fn elsewhere_here(report: &Connectivity) -> usize {
    report
        .roster
        .as_ref()
        .map_or(0, polis_ingest::Roster::live_elsewhere)
}

/// The panel that replaces an unexplained empty map.
///
/// > The operator ran a real agent for minutes and saw an empty map with no
/// > explanation.
///
/// Drawn while nothing is on the map. It says what is connected, what each
/// missing source would add, how many agents are visible on disk but not here,
/// and the one thing the operator has to do — which is nothing, in any terminal.
///
/// The repository is deliberately not named again: it is in the title bar three
/// lines above, and repeating a ninety-character path here is what pushed the
/// rows that matter off the bottom of the panel.
pub fn waiting(ui: &mut egui::Ui, feed: &LiveFeed, now: Instant) {
    let report = feed.report();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new("waiting for an agent…")
                        .monospace()
                        .strong()
                        .color(palette::needs_decision().color()),
                );
                ui.label(
                    RichText::new(format!(
                        "nothing for {}. Start `claude` here in any terminal — no flags, \
                         no setup — and it appears.",
                        crate::format::duration(feed.quiet_for(now))
                    ))
                    .small()
                    .color(palette::worker().color()),
                );
            });
            for line in &report.lines {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("{:<10}", line.name))
                            .monospace()
                            .small()
                            .color(reach_colour(line.reach)),
                    );
                    ui.label(dim(line.detail.clone()));
                    if let Some(hint) = &line.hint {
                        ui.label(
                            RichText::new(format!("→ {hint}"))
                                .small()
                                .color(palette::hover().color()),
                        );
                    }
                });
            }
            // Agents that exist and are not here. "Nothing is happening" and
            // "three agents are running, in another checkout" are different
            // situations, and not being able to tell them apart is what
            // produced this milestone.
            let elsewhere = elsewhere_here(report);
            if elsewhere > 0 {
                ui.label(dim(format!(
                    "{elsewhere} live elsewhere on this machine — reported, not drawn, \
                     because one window maps one repository. Open another with  polis -C \
                     <that repository> watch",
                )));
            }
        });
}

/// The colour a connectivity row is drawn in.
fn reach_colour(reach: Reach) -> egui::Color32 {
    match reach {
        Reach::Live => palette::status(ThreadStatus::Done).color(),
        Reach::Partial => palette::needs_decision().color(),
        Reach::Off => palette::status(ThreadStatus::Idle).color(),
    }
}

fn dim(text: impl Into<String>) -> RichText {
    RichText::new(text.into())
        .small()
        .color(palette::status(ThreadStatus::Idle).color())
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use polis_events::{
        EventKind, EventMeta, FsEvent, HookEvent, HookPayload, TranscriptEvent,
        TranscriptRecordKind, TranscriptSource,
    };
    use polis_ingest::ChannelSet;

    use super::*;

    fn mapper() -> PathMapper {
        PathMapper::new(Path::new(r"C:\coding\agentolis")).expect("a drive path is mappable")
    }

    fn hook(session: &str, cwd: Option<&str>) -> Event {
        let payload = HookPayload {
            session_id: Some(SessionId::new(session)),
            cwd: cwd.map(ToOwned::to_owned),
            ..HookPayload::default()
        };
        Event::new(
            EventMeta::now(Channel::Hook).with_session(SessionId::new(session)),
            Payload::Hook(Box::new(HookEvent {
                kind: EventKind::PreToolUse,
                truncated: false,
                payload,
            })),
        )
    }

    /// An OTel-shaped event: a session, and no `cwd` anywhere.
    fn telemetry(session: &str) -> Event {
        Event::new(
            EventMeta::now(Channel::Otel).with_session(SessionId::new(session)),
            Payload::Fs(FsEvent::RescanRequired),
        )
    }

    fn transcript(session: &str, cwd: &str) -> Event {
        Event::new(
            EventMeta::now(Channel::Transcript).with_session(SessionId::new(session)),
            Payload::Transcript(Box::new(TranscriptEvent {
                kind: TranscriptRecordKind::User,
                source: TranscriptSource::Main,
                byte_offset: 0,
                record: serde_json::json!({ "cwd": cwd }),
            })),
        )
    }

    fn drain(filter: &mut ScopeFilter, events: Vec<Event>) -> (Vec<Event>, usize) {
        let mut ready = Vec::new();
        let mut dropped = 0;
        for event in events {
            if filter.admit(event, &mut ready) == Verdict::Drop {
                dropped += 1;
            }
        }
        (ready, dropped)
    }

    /// The whole reason this gate exists on top of `SessionScope`: hooks
    /// registered in `~/.claude/settings.json` fire for every repository, and
    /// another checkout's `src/main.rs` shares a logical path with this one's —
    /// so two unrelated agents would render as one contention over one building.
    #[test]
    fn an_agent_in_another_checkout_never_reaches_this_city() {
        let mut filter = ScopeFilter::new(mapper(), Scope::Repo);
        let (ready, dropped) = drain(
            &mut filter,
            vec![
                hook("mine", Some(r"C:\coding\agentolis")),
                hook("theirs", Some(r"C:\coding\qurio-toolset")),
                hook("mine", None),
                hook("theirs", None),
            ],
        );
        assert_eq!(ready.len(), 2, "both of this checkout's events");
        assert_eq!(dropped, 2, "both of the other checkout's");
        for event in &ready {
            assert_eq!(session_of(event).map(SessionId::as_str), Some("mine"));
        }
    }

    /// A subdirectory is the repository, and a sibling directory with the same
    /// textual prefix is not — the classic `\repo` / `\repo-backup` false
    /// positive.
    #[test]
    fn scope_is_a_component_prefix_and_not_a_string_prefix() {
        let mut filter = ScopeFilter::new(mapper(), Scope::Repo);
        let (ready, dropped) = drain(
            &mut filter,
            vec![
                hook("deep", Some(r"C:\coding\agentolis\polis-app\src")),
                hook("neighbour", Some(r"C:\coding\agentolis-backup")),
                // Windows paths compare case-insensitively.
                hook("shouty", Some(r"C:\CODING\AGENTOLIS")),
            ],
        );
        assert_eq!(ready.len(), 2);
        assert_eq!(dropped, 1);
    }

    /// Telemetry arrives before the transcript record that says where the
    /// session is working. It must not be lost, and it must not arrive after the
    /// event that identified it.
    #[test]
    fn events_that_precede_the_verdict_are_held_and_released_in_order() {
        let mut filter = ScopeFilter::new(mapper(), Scope::Repo);
        let mut ready = Vec::new();
        assert_eq!(
            filter.admit(telemetry("s"), &mut ready),
            Verdict::Held,
            "no cwd yet"
        );
        assert_eq!(
            filter.admit(telemetry("s"), &mut ready),
            Verdict::Held,
            "still no cwd"
        );
        assert!(ready.is_empty());

        assert_eq!(
            filter.admit(transcript("s", r"C:\coding\agentolis"), &mut ready),
            Verdict::Apply
        );
        assert_eq!(ready.len(), 3, "the two held ones and the one that told us");
        assert_eq!(ready[0].meta.channel, Channel::Otel);
        assert_eq!(ready[1].meta.channel, Channel::Otel);
        assert_eq!(
            ready[2].meta.channel,
            Channel::Transcript,
            "the verdict's own event comes last"
        );

        // And the rest of the session flows straight through.
        ready.clear();
        assert_eq!(filter.admit(telemetry("s"), &mut ready), Verdict::Apply);
        assert_eq!(ready.len(), 1);
    }

    /// The mirror image: events held for a session that turns out to be someone
    /// else's are dropped, not applied.
    #[test]
    fn events_held_for_a_foreign_session_are_dropped_when_the_verdict_lands() {
        let mut filter = ScopeFilter::new(mapper(), Scope::Repo);
        let mut ready = Vec::new();
        assert_eq!(filter.admit(telemetry("s"), &mut ready), Verdict::Held);
        assert_eq!(
            filter.admit(transcript("s", r"D:\elsewhere"), &mut ready),
            Verdict::Drop
        );
        assert!(ready.is_empty(), "{ready:?}");
        assert!(filter.held.is_empty(), "the hold was swept");
    }

    /// A silent empty map is the failure this milestone exists to fix, so a
    /// session that never says where it is working is shown rather than hidden —
    /// and counted, so the strip can say so.
    #[test]
    fn a_session_that_never_reveals_a_cwd_is_let_through_and_counted() {
        let mut filter = ScopeFilter::new(mapper(), Scope::Repo);
        let mut ready = Vec::new();
        assert_eq!(filter.admit(telemetry("s"), &mut ready), Verdict::Held);
        assert!(ready.is_empty());

        // Nothing yet: the grace period has not passed.
        filter.expire(Instant::now(), &mut ready);
        assert!(ready.is_empty());

        filter.expire(
            Instant::now() + SCOPE_GRACE + Duration::from_millis(1),
            &mut ready,
        );
        assert_eq!(ready.len(), 1, "fail open, never fail silent");
        assert_eq!(filter.unscoped, 1);

        // And the session is remembered, so it is not re-held every grace period.
        ready.clear();
        assert_eq!(filter.admit(telemetry("s"), &mut ready), Verdict::Apply);
    }

    /// The hold is bounded: a machine with Channel D down must not grow a queue
    /// of held events without limit.
    #[test]
    fn the_hold_is_bounded_by_its_cap_as_well_as_by_time() {
        let mut filter = ScopeFilter::new(mapper(), Scope::Repo);
        let mut ready = Vec::new();
        for _ in 0..SCOPE_HOLD_CAP {
            let _ = filter.admit(telemetry("s"), &mut ready);
        }
        assert!(ready.is_empty());
        // Well inside the grace period, so only the cap can release these.
        filter.expire(Instant::now(), &mut ready);
        assert_eq!(ready.len(), SCOPE_HOLD_CAP);
    }

    /// Channel C is rooted at this checkout by the watcher and the control
    /// channel is Polis's own health. Neither has a session and neither may be
    /// scoped away.
    #[test]
    fn the_channels_with_no_session_are_never_scoped_away() {
        let mut filter = ScopeFilter::new(mapper(), Scope::Repo);
        let mut ready = Vec::new();
        let fs = Event::new(
            EventMeta::now(Channel::Fs),
            Payload::Fs(FsEvent::RescanRequired),
        );
        let control = Event::control(polis_events::ControlEvent::ChannelDegraded {
            channel: Channel::Otel,
            reason: "port taken".to_owned(),
        });
        assert_eq!(filter.admit(fs, &mut ready), Verdict::Apply);
        assert_eq!(filter.admit(control, &mut ready), Verdict::Apply);
        assert_eq!(ready.len(), 2);
    }

    /// `Scope::Machine` is the escape hatch, and it must hold nothing.
    #[test]
    fn machine_scope_admits_every_session_immediately() {
        let mut filter = ScopeFilter::new(mapper(), Scope::Machine);
        let (ready, dropped) = drain(
            &mut filter,
            vec![telemetry("a"), hook("b", Some(r"D:\elsewhere"))],
        );
        assert_eq!(ready.len(), 2);
        assert_eq!(dropped, 0);
        assert!(filter.held.is_empty());
    }

    /// An event storm costs frames, never frame rate: whatever is left after the
    /// per-frame ceiling stays in the carry, and the carry is what asks for the
    /// next frame.
    ///
    /// The accounting is exact on purpose. The first version of `pump` honoured
    /// the budget over the *classification* loop and then applied the whole
    /// batch afterwards, which is nanoseconds bounded and microseconds
    /// unbounded: a real watch drew a 314 ms frame with a 6 ms budget. Nothing
    /// may leave this call unapplied except through the carry.
    #[test]
    fn a_storm_is_capped_per_frame_and_the_remainder_is_carried() {
        let mut options = scratch_options();
        options.scope = Scope::Machine;
        let mut feed = LiveFeed::start(&options);
        let mut world = World::for_replay(polis_layout::CityLayout::default());
        let total = MAX_PER_PUMP + 500;
        feed.carry = (0..total).map(|_| telemetry("s")).collect();

        let first = feed.pump(&mut world, Instant::now());
        assert!(
            first.applied + first.filtered <= MAX_PER_PUMP,
            "one frame took more than its ceiling: {first:?}"
        );
        assert_eq!(
            first.applied + first.filtered + first.backlog,
            total,
            "every event is applied, filtered or carried: {first:?}"
        );
        assert!(feed.ready.is_empty(), "nothing is applied after the budget");
        assert_eq!(
            feed.wake_after(
                &WorldSnapshot::empty(Arc::new(polis_layout::CityLayout::default())),
                first.backlog
            ),
            Duration::ZERO,
            "a backlog wants the next frame now"
        );

        // And the carry is what the next frame eats, without another drain.
        let second = feed.pump(&mut world, Instant::now());
        assert!(second.applied > 0, "{second:?}");
        feed.shutdown();
    }

    /// A stack on ephemeral ports and no channels: never the real 4317 or
    /// 45177, because a listener on the hook port publishes the endpoint file
    /// every real agent on this machine reads.
    fn scratch_options() -> LiveOptions {
        let mut options = LiveOptions::for_repo(std::env::temp_dir());
        options.ingest.otlp_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
        options.ingest.hook_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        options.ingest.channels = ChannelSet::NONE;
        options
    }

    /// PRD §5: one drain, one apply pass, one publish per frame — and the world
    /// is aged even on a frame where nothing arrived, or a thread would never
    /// go idle and a trail would never fade on a quiet map.
    #[test]
    fn a_pump_with_no_events_still_ages_the_world_and_asks_for_a_slow_frame() {
        let mut feed = LiveFeed::start(&scratch_options());
        let mut world = World::for_replay(polis_layout::CityLayout::default());
        let before = world.now();
        let pumped = feed.pump(&mut world, before + Duration::from_secs(1));
        assert_eq!(pumped, Pumped::default());
        assert!(world.now() > before, "tick runs on an empty frame too");
        assert!(!feed.has_seen_an_event());
        assert_eq!(feed.dropped(), 0, "PRD §4.5's counter is always a number");

        let snapshot = WorldSnapshot::empty(Arc::new(polis_layout::CityLayout::default()));
        assert_eq!(feed.wake_after(&snapshot, 0), IDLE_TICK);
        feed.shutdown();
    }
}
