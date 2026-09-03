//! `polis-world` — world state (PRD §5, §6, §11, §14).
//!
//! > Single-writer, multi-reader. One thread owns `World` and applies events;
//! > the renderer reads a lock-free snapshot (`arc-swap`) published at most once
//! > per frame.
//!
//! > **Decouple event rate from frame rate.** Never render on event arrival.
//! > Events mutate `World`; the renderer samples at its own cadence.
//!
//! | Module | PRD section |
//! |---|---|
//! | [`territory`] | §6 territory inference — evidence weights, convergence, hysteresis, KDE |
//! | [`contention`] | §11.3 the claim table |
//! | [`attention`] | §11.1–§11.2 the three states and their ordering |
//! | [`snapshot`] | §5 the `arc-swap` snapshot the renderer reads |
//! | [`replay`] | §15 M2 — the offline transcript driver and its transport clock |
//! | [`sessions`] | §15 M2 — the index of the operator's own sessions |
//! | [`verify`] | §11.2 — which shell commands count as "tests ran" |
//!
//! # The shape of the API, for the three crates that render it
//!
//! There are exactly two ways to move the world forward, and one way to read it:
//!
//! 1. [`World::apply`] consumes one [`Event`]. It is the **only** mutation path
//!    for incoming data.
//! 2. [`World::tick`] advances decay, TTLs and hysteresis to an instant. Call it
//!    once per batch, never per event, so a chatty fleet does not age the world
//!    faster than a quiet one.
//! 3. [`snapshot::SnapshotReader::load`] returns the newest published
//!    [`snapshot::WorldSnapshot`]. The reader holds no reference to [`World`],
//!    no channel back to the world thread, and no method that computes
//!    anything — so a renderer *physically cannot* force a recompute. It samples
//!    whatever was last published and draws that.
//!
//! # The M2 loop, end to end
//!
//! Pick a session, read it, build a world, drive it, draw whatever was last
//! published. Nothing here needs a running agent, a collector, or any
//! configuration at all.
//!
//! ```no_run
//! use std::time::Duration;
//! use polis_events::PathMapper;
//! use polis_world::replay::{ReplayDriver, DEFAULT_IDLE_GAP_CAP};
//! use polis_world::sessions::{IndexOptions, SessionIndex};
//! use polis_world::{snapshot, World};
//!
//! # fn main() -> anyhow::Result<()> {
//! // 1. What has the operator actually got? Newest first.
//! let projects = polis_world::sessions::default_projects_dir().expect("a home directory");
//! let index = SessionIndex::scan_with(&projects, &IndexOptions::quick())?;
//! let session = index.replayable().next().expect("one of your own sessions");
//!
//! // 2. The city it ran in, and the events it produced.
//! let repo = polis_repo::tree::RepoIndex::open(session.repo.as_ref().unwrap())?;
//! let layout = polis_layout::city::generate(repo.tree());
//! let mapper = PathMapper::new(session.repo.as_ref().unwrap())?;
//! let schedule = session
//!     .schedule(&mapper)?
//!     .with_idle_gap_cap(Some(DEFAULT_IDLE_GAP_CAP));
//!
//! // 3. One world, one publisher, one reader.
//! let mut world = World::new(repo.tree().clone(), layout);
//! let (publisher, reader) = snapshot::from_world(&world);
//! let mut driver = ReplayDriver::new(schedule);
//! driver.clock_mut().play();
//!
//! // 4. Once per frame: advance, publish, draw.
//! while !driver.is_finished() {
//!     driver.advance(Duration::from_millis(16), &mut world);
//!     publisher.publish(&world);
//!     let frame = reader.load();
//!     let _ = (frame.threads.len(), driver.progress().fraction());
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Clocks
//!
//! Every instant in this crate is a [`std::time::Instant`] taken from
//! [`polis_events::EventMeta::observed`] — the receipt clock (ADR-0014).
//! Transcript timestamps are display-only and never reach a decay calculation:
//! 20% of transcript files step backwards, and one observed jump was 60 seconds.
//! During replay the same rule holds, because
//! [`polis_events::ReplayClock`] rebases recorded offsets onto a fresh monotonic
//! origin before the events reach [`World::apply`] — the world cannot tell a
//! replay from a live session, which is the entire point of M2 being a first
//! class mode.
//!
//! # Degrading honestly
//!
//! Where a signal is unavailable this crate says so rather than inventing a
//! value. [`Health`] carries the counters, [`FileState::diff_precision`] says
//! whether a diff count is exact or approximated, and
//! [`contention::Contention::precision`] says whether a severity tier had line
//! ranges to work with — because `structuredPatch` is absent on 65% of subagent
//! tool results (ADR-0004) and fabricating precision there would make the one
//! state where work is actively being destroyed the least trustworthy thing on
//! the map.

pub mod attention;
pub mod contention;
pub mod place;
pub mod replay;
pub mod sessions;
pub mod snapshot;
pub mod territory;
pub mod verify;

mod apply;

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::time::{Duration, Instant};

use polis_events::{
    AgentType, Event, Glyph, LogicalPath, Outcome, PathMapper, Payload, SessionId, ThreadId,
    ToolKind, ToolUseId, WorkerId, WorktreeId,
};
use polis_layout::{CityLayout, Point};
use polis_repo::corpus::{Corpus, Denylist};
use polis_repo::RepoTree;
use smallvec::SmallVec;

pub use apply::PendingCall;
pub use place::{OpPlacement, OpSite, PlacementCensus};

// ---------------------------------------------------------------------------
// Tuning constants
// ---------------------------------------------------------------------------

/// How many trail steps a thread keeps (PRD §12).
///
/// The cap bounds memory, not meaning: [`Thread::visits`] keeps the revisit
/// count for every path the thread ever touched, so "the same building revisited
/// six times" survives the hundred-and-ninety-third step falling off the end.
///
/// # Why it is not 64
///
/// PRD §12 asks the trail to show *"backtracking, thrashing (the same building
/// revisited six times), and scope creep"*. A backtrack is only visible if the
/// **outbound leg is still on the trail when the agent returns**, and 64 was
/// below the distance a real backtrack covers. Measured over the operator's
/// three recorded sessions — 2 765 revisits in all — the number of trail steps
/// between two consecutive visits to one path has a median of 9–31 and a 90th
/// percentile of 97–418. Against that distribution:
///
/// | cap | TTL | round trips fully on the trail |
/// |---:|---:|---|
/// | 64 | 300 s | 51.8% / 46.3% / 69.5% |
/// | 192 | 900 s | **67.6% / 61.8% / 80.2%** |
/// | 256 | 900 s | 68.3% / 61.8% / 80.2% |
///
/// 192 is the knee; 256 buys half a point. The renderer's cost is bounded
/// separately, by fading and by the per-thread mark cap, not by this.
pub const TRAIL_CAP: usize = 192;

/// How long a trail step stays on the map before [`World::tick`] drops it.
///
/// > **Trails persist and fade** whether or not you are following, giving you
/// > history without a timeline scrubber. (PRD §12)
///
/// The other half of [`TRAIL_CAP`]'s argument, and the half that bit hardest:
/// at 300 s the *time* between two visits to one path exceeded the TTL on 54% /
/// 42% / 31% of real revisits, so the outbound leg had expired before the agent
/// came back and the backtrack was drawn as a lone arrival. 900 s is fifteen
/// minutes of history — long enough to hold a real detour, short enough that
/// the oldest steps have faded most of the way to the band floor by then.
pub const TRAIL_TTL: Duration = Duration::from_mins(15);

/// How many glyph-bearing operations a thread keeps for the §10.1 shape layer.
pub const OPS_CAP: usize = 128;

/// How long a thread may be silent before [`ThreadStatus::Working`] decays to
/// [`ThreadStatus::Idle`].
///
/// Idle is "alive but quiet", not "finished": only an explicit stop signal
/// produces [`ThreadStatus::Done`].
pub const IDLE_AFTER: Duration = Duration::from_secs(60);

/// Evidence weight of an `@`-mentioned file (ADR-0017).
///
/// **Not from PRD §6.1's table**, which only covers tool calls. A file the
/// operator pinned with `@` is added to context with no tool call at all, so it
/// is invisible to §6.1 — "a territory that ignores `@`-mentions systematically
/// under-weights exactly the files the operator considered most relevant". It is
/// weighted as commitment (3.0) rather than as orientation (1.0) because the
/// operator chose it deliberately.
pub const AT_MENTION_WEIGHT: f32 = 3.0;

// ---------------------------------------------------------------------------
// The world
// ---------------------------------------------------------------------------

/// Everything Polis knows (PRD §5).
///
/// `BTreeMap` throughout, not `HashMap`: PRD §7.4 forbids iteration order from
/// reaching anything the layout or a golden file can see.
///
/// One thread owns this. It is neither `Sync` nor internally locked; readers go
/// through [`snapshot::SnapshotReader`].
#[derive(Debug)]
pub struct World {
    /// Logical paths, sizes, git metadata.
    pub repo: RepoTree,
    /// The generated city. Changes rarely.
    pub layout: CityLayout,
    /// One entry per main agent and its worker subtree.
    pub threads: BTreeMap<ThreadId, Thread>,
    /// Per-file live state.
    pub files: BTreeMap<LogicalPath, FileState>,
    /// §11.3 contention claims.
    pub claims: contention::ClaimTable,
    /// §11.2 attention marks, **ordered by severity**:
    /// `contention > needs-decision > done`.
    pub attention: Vec<attention::Attention>,
    /// Channel health for the status bar (PRD §4.5, §17).
    pub health: Health,
    /// Workers seen with no thread to attribute them to.
    ///
    /// Modelled explicitly rather than guessed at: a worker whose parent link is
    /// missing is *not* silently folded into the main agent, because that would
    /// make one thread's territory absorb another's evidence. The UI shows these
    /// in the status rail as unplaced, which is what uncertainty looks like.
    pub unattributed: BTreeMap<WorkerId, UnattributedWorker>,

    /// Absolute path → `(worktree, logical path)` (PRD §7.6).
    mapper: PathMapper,
    /// The TF-IDF ubiquity discount of PRD §6.1.
    ubiquity: Box<dyn Ubiquity>,
    /// In-flight tool calls, keyed by the cross-channel join key.
    pending: BTreeMap<ToolUseId, PendingCall>,
    /// `wf_<runId>` → the thread that issued the `Workflow` call (ADR-0013
    /// route 3, the only parent link 503 of 643 subagents have).
    workflow_runs: BTreeMap<String, ThreadId>,
    /// `toolu_…` of an `Agent` call → who issued it, for ADR-0013 route 1.
    spawns: BTreeMap<ToolUseId, ThreadId>,
    /// Whether an unrecognised `cwd` may be adopted as a worktree.
    adopt_worktrees: bool,
    /// Highest `observed` seen, so `tick` has a floor even before the first one.
    now: Instant,
    /// Bumped whenever [`Self::layout`] is replaced.
    layout_generation: u64,
    /// Bumped whenever [`Self::files`] changes.
    files_generation: u64,
}

impl World {
    /// Builds an empty world over a repository and its city.
    ///
    /// The [`PathMapper`] is derived from the repository root and its registered
    /// worktrees. A repository root that is not valid UTF-8 leaves the mapper
    /// empty and records a degraded channel rather than failing: every absolute
    /// path then maps to nothing, which is a city with no live layer rather than
    /// no city.
    pub fn new(repo: RepoTree, layout: CityLayout) -> Self {
        let mut health = Health::default();
        let mut mapper = PathMapper::default();
        if repo.root.as_os_str().is_empty() {
            // No repository: a replay-only world. The first `cwd` seen becomes
            // the primary checkout.
        } else if let Err(e) = mapper.add_worktree(WorktreeId::PRIMARY, &repo.root) {
            health.degraded.insert(
                "paths".to_owned(),
                format!("repository root is not a usable path: {e}"),
            );
        }
        for (id, root) in &repo.worktrees {
            if *id != WorktreeId::PRIMARY {
                let _ = mapper.add_worktree(*id, root);
            }
        }
        Self {
            repo,
            layout,
            threads: BTreeMap::new(),
            files: BTreeMap::new(),
            claims: contention::ClaimTable::default(),
            attention: Vec::new(),
            health,
            unattributed: BTreeMap::new(),
            mapper,
            ubiquity: Box::new(NoUbiquity),
            pending: BTreeMap::new(),
            workflow_runs: BTreeMap::new(),
            spawns: BTreeMap::new(),
            adopt_worktrees: true,
            now: Instant::now(),
            layout_generation: 0,
            files_generation: 0,
        }
    }

    /// A world with no repository index, for replaying a session over a city
    /// that was generated elsewhere (PRD §15 M2).
    ///
    /// The session's own `cwd` becomes the primary checkout the first time a
    /// record carries one, which is what makes `polis replay <transcript>` work
    /// with nothing configured.
    pub fn for_replay(layout: CityLayout) -> Self {
        Self::new(RepoTree::default(), layout)
    }

    /// Applies one event. The only mutation path.
    ///
    /// Unknown payloads, unknown tools and unknown record types are counted in
    /// [`Health::drift`] and dropped — never fatal (PRD §4.1).
    pub fn apply(&mut self, event: &Event) {
        if event.meta.observed > self.now {
            self.now = event.meta.observed;
        }
        self.health.events_applied += 1;
        match &event.payload {
            Payload::Otel(e) => apply::otel(self, &event.meta, e),
            Payload::Metric(m) => apply::metric(self, m),
            Payload::Hook(h) => apply::hook(self, &event.meta, h),
            Payload::Fs(f) => apply::fs(self, &event.meta, f),
            Payload::Transcript(t) => apply::transcript(self, &event.meta, t),
            Payload::Control(c) => apply::control(self, c),
            // `Payload` is `#[non_exhaustive]` (ADR-0048): a fifth channel is a
            // drift signal, not a compile break and not a panic.
            other => {
                tracing::debug!(payload = ?std::mem::discriminant(other), "unmodelled payload");
                self.health.drift += 1;
                self.health.events_ignored += 1;
            }
        }
    }

    /// Advances decay, TTLs and hysteresis to `now`.
    ///
    /// Called once per batch, not per event, so territory half-life and claim
    /// expiry do not depend on how chatty the fleet is.
    pub fn tick(&mut self, now: Instant) {
        if now > self.now {
            self.now = now;
        }
        let now = self.now;

        self.claims.expire(now);
        for thread in self.threads.values_mut() {
            thread.territory.decay(now);
            thread.fade_trail(now);
            if thread.status == ThreadStatus::Working
                && now.saturating_duration_since(thread.last_activity) > IDLE_AFTER
            {
                thread.status = ThreadStatus::Idle;
            }
        }
        self.refresh_contention(now);
        attention::expire(&mut self.attention, now);
        attention::sort(&mut self.attention);
    }

    /// Clears every live layer and rebases the clock to `now`, keeping the
    /// repository, the city, the path mapper and the ubiquity source.
    ///
    /// This is what makes a backwards seek possible in [`replay`]: events are
    /// not invertible, so seeking to an earlier moment resets and re-applies.
    ///
    /// **`now` is not optional and not `Instant::now()`.** The world's clock is
    /// monotonic — [`World::tick`] never moves it backwards — so a reset that
    /// left it at the moment the seek started would age every re-applied event
    /// by the whole span being rewound, and the rebuilt world would arrive with
    /// its trails already faded. Replay passes its monotonic origin.
    pub fn reset(&mut self, now: Instant) {
        self.now = now;
        self.threads.clear();
        self.files.clear();
        self.claims = contention::ClaimTable::default();
        self.attention.clear();
        self.unattributed.clear();
        self.pending.clear();
        self.workflow_runs.clear();
        self.spawns.clear();
        self.health.reset_counters();
        self.files_generation = self.files_generation.wrapping_add(1);
    }

    /// The path mapper (PRD §7.6).
    pub fn mapper(&self) -> &PathMapper {
        &self.mapper
    }

    /// The path mapper, for registering worktrees discovered by
    /// `git worktree list` or by a `CwdChanged` hook.
    pub fn mapper_mut(&mut self) -> &mut PathMapper {
        &mut self.mapper
    }

    /// Replaces the city, bumping the generation the snapshot publisher keys its
    /// `Arc` cache on.
    ///
    /// > **Never move the ground while the operator is looking at it.**
    /// > (PRD §7.7) — that tween is the renderer's job; this is where the new
    /// > ground arrives.
    pub fn set_layout(&mut self, layout: CityLayout) {
        self.layout = layout;
        self.layout_generation = self.layout_generation.wrapping_add(1);
    }

    /// Installs the PRD §6.1 ubiquity discount.
    ///
    /// Without one every observation is undiscounted, which means the README and
    /// `package.json` weigh as much as `src/auth/token.rs` and every territory
    /// drifts toward the repository root.
    pub fn set_ubiquity(&mut self, ubiquity: Box<dyn Ubiquity>) {
        self.ubiquity = ubiquity;
    }

    /// Whether an unrecognised `cwd` is adopted as an additional worktree.
    ///
    /// On by default, because PRD §7.6's whole point is that
    /// `/repo-wt-3/src/auth.ts` and `/repo-wt-7/src/auth.ts` are one logical
    /// file: an adopted worktree makes both land on the same building. Turn it
    /// off when replaying a session recorded against a *different* repository,
    /// where the collision would be a lie rather than the truth.
    pub fn set_adopt_worktrees(&mut self, adopt: bool) {
        self.adopt_worktrees = adopt;
    }

    /// The latest instant the world has been advanced to.
    pub fn now(&self) -> Instant {
        self.now
    }

    /// One thread.
    pub fn thread(&self, id: &ThreadId) -> Option<&Thread> {
        self.threads.get(id)
    }

    /// One file's live state.
    pub fn file(&self, path: &LogicalPath) -> Option<&FileState> {
        self.files.get(path)
    }

    /// Where a logical path sits in city space.
    ///
    /// A file answers with its building's centroid; a directory answers with its
    /// district centre. `None` for a path with no geometry — a file outside the
    /// repository, or one added since the last layout run — which is a normal
    /// condition and the reason [`territory::Territory::observe`] takes an
    /// `Option<Point>`.
    pub fn position_of(&self, path: &LogicalPath) -> Option<Point> {
        place::position_in(&self.layout, path)
    }

    /// Where one operation is drawn — [`place`]'s four-rung chain, resolved
    /// against this world's city and the thread that ran it.
    ///
    /// `None` for a thread this world does not know, which is a caller error
    /// rather than a placement result; an operation whose thread exists always
    /// gets an answer, and that answer may be [`OpSite::Rail`].
    pub fn site_of(&self, op: &Operation, thread: &ThreadId) -> Option<OpSite> {
        let t = self.threads.get(thread)?;
        Some(place::site_of(op, t, &self.layout))
    }

    /// Threads in status order — waiting first, then working, then idle, then
    /// done; ties broken by most recent activity.
    ///
    /// This is the status-rail order, and it is computed here rather than in the
    /// UI so every surface agrees on it.
    pub fn threads_for_rail(&self) -> Vec<&Thread> {
        let mut out: Vec<&Thread> = self.threads.values().collect();
        out.sort_by(|a, b| {
            a.status
                .rail_rank()
                .cmp(&b.status.rail_rank())
                .then(b.last_activity.cmp(&a.last_activity))
                .then(a.id.cmp(&b.id))
        });
        out
    }

    /// Every path the thread has touched, for
    /// [`polis_repo::corpus::Corpus::record_session`] at session end.
    pub fn paths_touched_by(&self, thread: &ThreadId) -> Vec<LogicalPath> {
        self.threads
            .get(thread)
            .map(|t| t.visits.keys().cloned().collect())
            .unwrap_or_default()
    }

    // -- internals shared with `apply` -------------------------------------

    /// The thread for a session, created if this is the first time it is seen.
    pub(crate) fn thread_entry(&mut self, session: &SessionId, at: Instant) -> &mut Thread {
        let id = ThreadId::of_session(session.clone());
        self.threads
            .entry(id.clone())
            .or_insert_with(|| Thread::new(id, session.clone(), at))
    }

    /// Resolves a raw path from a tool input against the mapper.
    ///
    /// Counts an unmapped path in [`Health::unmapped_paths`] rather than
    /// dropping it silently: a mapper with the wrong root produces a live layer
    /// that is empty and looks healthy, which is the failure ADR-0011 exists to
    /// prevent.
    pub(crate) fn resolve_path(
        &mut self,
        cwd: Option<&str>,
        raw: &str,
    ) -> Option<(WorktreeId, LogicalPath)> {
        let cwd_path = cwd.map(std::path::Path::new);
        if let Some(hit) = self.mapper.resolve(cwd_path, raw) {
            return Some(hit);
        }
        // A relative path with no cwd, or an absolute path outside every root.
        if let Ok(rel) = LogicalPath::new(raw) {
            if !rel.is_root() && cwd.is_none() {
                return Some((WorktreeId::PRIMARY, rel));
            }
        }
        self.health.unmapped_paths += 1;
        None
    }

    /// Rungs 1–3 of [`place`]'s chain, decided from what a call named.
    ///
    /// `own` is the first path the tool's own input resolved to; `cwd` is the
    /// working directory a shell call ran in. Rung 4 is deliberately not decided
    /// here — see [`OpPlacement`].
    pub(crate) fn placement_for(
        &self,
        own: Option<&LogicalPath>,
        cwd: Option<&LogicalPath>,
    ) -> OpPlacement {
        if let Some(p) = own {
            if place::position_in(&self.layout, p).is_some() {
                return OpPlacement::Path(p.clone());
            }
        }
        if let Some(c) = cwd {
            if let Some(district) = place::district_for_cwd(&self.layout, c) {
                return OpPlacement::Cwd(district);
            }
        }
        OpPlacement::Agent
    }

    /// Counts one operation into [`Health::ops`] on the rung it actually
    /// resolved to, failures into [`Health::ops_failed`] as well.
    ///
    /// Called with the thread already in place, because rung 3 versus rung 4 is
    /// a question about the thread, not about the call.
    pub(crate) fn census_op(&mut self, thread: &ThreadId, op: &Operation) {
        let rung = self.rung_of(thread, op);
        self.health.ops.count(rung);
        if op.outcome == Outcome::Failed {
            self.health.ops_failed.count(rung);
        }
    }

    /// Counts an operation that turned out to have failed, once its result
    /// arrived. The transcript channel learns the outcome long after the call.
    pub(crate) fn census_op_failed(&mut self, thread: &ThreadId, op: &Operation) {
        let rung = self.rung_of(thread, op);
        self.health.ops_failed.count(rung);
    }

    /// The rung [`place`]'s chain resolves this operation to right now.
    fn rung_of(&self, thread: &ThreadId, op: &Operation) -> u8 {
        self.threads
            .get(thread)
            .map_or(4, |t| place::site_of(op, t, &self.layout).rung())
    }

    /// Registers a working directory, adopting it as a checkout when the mapper
    /// does not already cover it.
    pub(crate) fn note_cwd(&mut self, cwd: &str) -> Option<WorktreeId> {
        if let Some((id, _)) = self.mapper.to_logical_str(cwd) {
            return Some(id);
        }
        if !self.adopt_worktrees {
            return None;
        }
        let next = if self.mapper.roots().next().is_none() {
            WorktreeId::PRIMARY
        } else {
            let used: Vec<u32> = self.mapper.roots().map(|(id, _)| id.0).collect();
            WorktreeId(
                (0..u32::MAX)
                    .find(|n| !used.contains(n))
                    .unwrap_or(u32::MAX),
            )
        };
        match self.mapper.add_worktree(next, std::path::Path::new(cwd)) {
            Ok(()) => {
                if !next.is_primary() {
                    self.health.adopted_worktrees += 1;
                }
                Some(next)
            }
            Err(_) => None,
        }
    }

    /// Records an observation: territory evidence, trail, visit count.
    pub(crate) fn observe(&mut self, obs: &Observation) {
        let discount = self.ubiquity.discount(&obs.path);
        let at = self.position_of(&obs.path).or_else(|| {
            obs.path
                .parent()
                .and_then(|p| self.layout.districts.get(&p).map(|d| d.centre))
        });
        if at.is_none() {
            self.health.unplaced_observations += 1;
        }
        let base = self.layout.extent;
        let Some(thread) = self.threads.get_mut(&obs.thread) else {
            return;
        };
        if thread.territory.base_bandwidth <= 0.0 && base > 0.0 {
            thread.territory.base_bandwidth = base * territory::BANDWIDTH_FRACTION;
        }
        thread.territory.observe(obs, discount, at);
        thread.step_trail(obs.path.clone(), obs.at);
        thread.last_activity = obs.at;
        if let Some(worker) = &obs.worker {
            if let Some(w) = thread.worker_mut(worker) {
                w.focus = Some(obs.path.clone());
                w.last_activity = obs.at;
                w.ops = w.ops.saturating_add(1);
            }
        }
    }

    /// Touches a file, creating its state on first sight.
    pub(crate) fn file_entry(&mut self, path: &LogicalPath) -> &mut FileState {
        self.files_generation = self.files_generation.wrapping_add(1);
        self.files.entry(path.clone()).or_default()
    }

    /// Raises an attention mark, replacing an equivalent one rather than
    /// stacking duplicates.
    pub(crate) fn raise(&mut self, kind: attention::AttentionKind, at: Instant) {
        if let Some(existing) = self
            .attention
            .iter_mut()
            .find(|m| m.kind.same_subject(&kind))
        {
            existing.kind = kind;
            return;
        }
        self.attention
            .push(attention::Attention { kind, since: at });
        attention::sort(&mut self.attention);
    }

    /// Clears every "needs decision" mark for a thread — the human answered, or
    /// the thread made progress, which is the same evidence.
    pub(crate) fn resolve_decisions(&mut self, thread: &ThreadId) {
        self.attention.retain(|m| !m.kind.is_decision_for(thread));
    }

    /// Rebuilds the contention marks from the live claim table.
    ///
    /// Contention is a **relation**, not a property, so it cannot outlive the
    /// pair of claims that produced it: rebuilding is cheaper and more honest
    /// than trying to expire the marks separately.
    fn refresh_contention(&mut self, now: Instant) {
        let hits = self.claims.hits(now);
        self.attention
            .retain(|m| !matches!(m.kind, attention::AttentionKind::Contention(_)));
        for hit in hits {
            let since = hit.challenger.at;
            self.attention.push(attention::Attention {
                kind: attention::AttentionKind::Contention(Box::new(hit)),
                since,
            });
        }
    }

    /// The generation counters the snapshot publisher keys its `Arc` caches on.
    pub(crate) fn generations(&self) -> (u64, u64) {
        (self.layout_generation, self.files_generation)
    }
}

// ---------------------------------------------------------------------------
// Threads and workers
// ---------------------------------------------------------------------------

/// A main agent plus its worker subtree — the unit the operator thinks in
/// (PRD §3, §5).
#[derive(Debug, Clone)]
pub struct Thread {
    /// `(session_id)`. Workers inside it are keyed by [`WorkerId`].
    pub id: ThreadId,
    /// The underlying session.
    pub session_id: SessionId,
    /// Which checkout this thread is working in (PRD §7.6). A rendering
    /// dimension over the shared base map, never a separate city.
    pub worktree: Option<WorktreeId>,
    /// Inferred scope — a density field, not a boundary (PRD §6).
    pub territory: territory::Territory,
    /// Subagents.
    pub workers: Vec<Worker>,
    /// Recently touched paths, capped and decaying.
    ///
    /// > Trail is the natural intermediate representation between "architecture"
    /// > and "diff" — you can see backtracking, thrashing (the same building
    /// > revisited six times), and scope creep in it. (PRD §12)
    ///
    /// Capped at [`TRAIL_CAP`] steps and faded at [`TRAIL_TTL`]. The revisit
    /// counts the PRD is describing live in [`Thread::visits`] and are **not**
    /// lost when a step falls off the end.
    pub trail: VecDeque<(LogicalPath, Instant)>,
    /// Working / Waiting / Idle / Done.
    pub status: ThreadStatus,

    /// The same steps as [`Thread::trail`] with PRD §10.1's shape channel
    /// attached: which operation, how it went, and which worker did it.
    ///
    /// `trail` is the geometry the renderer follows; `ops` is what it draws at
    /// each stop. Kept separate because §10.1 is emphatic that shape and colour
    /// are orthogonal channels and the trail's own encoding is neither.
    pub ops: VecDeque<Operation>,
    /// Every path this thread has ever touched, with its revisit count.
    ///
    /// > you can see backtracking, thrashing (the same building revisited six
    /// > times), and scope creep in it (PRD §12)
    pub visits: BTreeMap<LogicalPath, VisitStats>,
    /// When the thread was first seen.
    pub started: Instant,
    /// When it last did anything.
    pub last_activity: Instant,
    /// `ai-title` / `custom-title` / session slug, when the transcript names one.
    pub title: Option<String>,
    /// `default` / `plan` / `acceptEdits` / `auto` / `dontAsk` /
    /// `bypassPermissions`. The UI's **Manual** mode arrives as `default`.
    pub permission_mode: Option<String>,
    /// Branch, or `None` when detached — `"HEAD"` is not a branch (ADR-0018).
    pub branch: Option<String>,
    /// Working directory as the records report it.
    pub cwd: Option<String>,
    /// The agent type of a main agent started with `claude --agent foo`.
    /// **Never** a worker discriminator (ADR-0030).
    pub agent_type: Option<AgentType>,
    /// Lines added by this thread, summed over what the channels reported.
    pub lines_added: u32,
    /// Lines removed.
    pub lines_removed: u32,
    /// Tool calls seen.
    pub tool_calls: u32,
    /// Tool calls that failed or were rejected.
    pub failures: u32,
    /// When this thread last ran something [`verify`] recognises as a test.
    pub last_verified: Option<Instant>,
}

impl Thread {
    /// A new thread, first seen at `at`.
    pub fn new(id: ThreadId, session_id: SessionId, at: Instant) -> Self {
        Self {
            id,
            session_id,
            worktree: None,
            territory: territory::Territory::default(),
            workers: Vec::new(),
            trail: VecDeque::new(),
            status: ThreadStatus::Working,
            ops: VecDeque::new(),
            visits: BTreeMap::new(),
            started: at,
            last_activity: at,
            title: None,
            permission_mode: None,
            branch: None,
            cwd: None,
            agent_type: None,
            lines_added: 0,
            lines_removed: 0,
            tool_calls: 0,
            failures: 0,
            last_verified: None,
        }
    }

    /// One worker of this thread.
    pub fn worker(&self, id: &WorkerId) -> Option<&Worker> {
        self.workers.iter().find(|w| &w.id == id)
    }

    /// One worker of this thread, mutably.
    pub fn worker_mut(&mut self, id: &WorkerId) -> Option<&mut Worker> {
        self.workers.iter_mut().find(|w| &w.id == id)
    }

    /// The worker for `id`, created if unseen. Workers stay in spawn order.
    pub(crate) fn worker_entry(&mut self, id: &WorkerId, at: Instant) -> &mut Worker {
        if let Some(idx) = self.workers.iter().position(|w| &w.id == id) {
            return &mut self.workers[idx];
        }
        self.workers.push(Worker::new(id.clone(), at));
        let last = self.workers.len() - 1;
        &mut self.workers[last]
    }

    /// How many times this thread has touched a path (PRD §12's thrashing
    /// signal).
    pub fn revisits(&self, path: &LogicalPath) -> u32 {
        self.visits.get(path).map_or(0, |v| v.count)
    }

    /// The path this thread has revisited most, and how often.
    ///
    /// Ties break on the path, so the answer does not depend on iteration
    /// order — the same rule PRD §7.4 imposes on the layout, applied here
    /// because this drives a label the operator reads.
    pub fn most_revisited(&self) -> Option<(&LogicalPath, u32)> {
        self.visits
            .iter()
            .max_by(|(pa, a), (pb, b)| a.count.cmp(&b.count).then_with(|| pb.cmp(pa)))
            .map(|(p, v)| (p, v.count))
    }

    /// How many workers are still running (ADR-0019: `async_launched` is not
    /// completion).
    pub fn running_workers(&self) -> usize {
        self.workers.iter().filter(|w| w.running).count()
    }

    /// Whether every file this thread edited has been verified since it was
    /// last touched.
    ///
    /// This is the split PRD §11.2 requires: *done, verified* decays, *done,
    /// unverified* is really "needs review" and persists. A thread that edited
    /// nothing is trivially verified.
    pub fn is_verified(&self, files: &BTreeMap<LogicalPath, FileState>) -> bool {
        self.visits
            .iter()
            .filter(|(_, v)| v.writes > 0)
            .all(|(path, _)| {
                files.get(path).is_some_and(
                    |f| matches!((f.last_verified, f.last_touched), (Some(v), Some(t)) if v >= t),
                )
            })
    }

    /// Appends a trail step and its revisit count.
    fn step_trail(&mut self, path: LogicalPath, at: Instant) {
        let entry = self.visits.entry(path.clone()).or_insert(VisitStats {
            count: 0,
            writes: 0,
            first: at,
            last: at,
        });
        entry.count = entry.count.saturating_add(1);
        entry.last = at;
        // A step that repeats the previous one refreshes it rather than filling
        // the trail with a stack of identical points; the revisit count above is
        // where the repetition is recorded.
        if self.trail.back().is_some_and(|(p, _)| p == &path) {
            if let Some(back) = self.trail.back_mut() {
                back.1 = at;
            }
            return;
        }
        self.trail.push_back((path, at));
        while self.trail.len() > TRAIL_CAP {
            self.trail.pop_front();
        }
    }

    /// Appends a §10.1 operation.
    pub(crate) fn push_op(&mut self, op: Operation) {
        self.ops.push_back(op);
        while self.ops.len() > OPS_CAP {
            self.ops.pop_front();
        }
    }

    /// Drops trail steps older than [`TRAIL_TTL`].
    fn fade_trail(&mut self, now: Instant) {
        while self
            .trail
            .front()
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) > TRAIL_TTL)
        {
            self.trail.pop_front();
        }
        while self
            .ops
            .front()
            .is_some_and(|op| now.saturating_duration_since(op.at) > TRAIL_TTL)
        {
            self.ops.pop_front();
        }
    }
}

/// A subagent (PRD §3).
#[derive(Debug, Clone)]
pub struct Worker {
    /// `agent_id`. Presence of this, and nothing else, is what makes a record a
    /// worker's.
    pub id: WorkerId,
    /// `Explore`, `Plan`, `general-purpose`, `workflow-subagent`, a custom
    /// frontmatter name. A type, never an identity.
    ///
    /// Empty when no channel has said yet; check with
    /// [`polis_events::AgentType::is_empty`] rather than assuming a value.
    pub kind: AgentType,
    /// Where the worker most recently acted, for the tether the renderer draws
    /// back to its thread.
    pub focus: Option<LogicalPath>,
    /// Whether the worker is still running.
    ///
    /// An async `Agent` spawn returns `status: "async_launched"` immediately
    /// while the subagent keeps working, and completion arrives much later as a
    /// separate record. A tailer that treats `async_launched` as terminal shows
    /// agents finishing before they start (ADR-0019).
    pub running: bool,
    /// How this worker was attached to its thread, and how strong that evidence
    /// is. Rendered as uncertainty, never hidden.
    pub attribution: WorkerAttribution,
    /// When the worker was first seen.
    pub started: Instant,
    /// When it last did anything.
    pub last_activity: Instant,
    /// Operations attributed to it.
    pub ops: u32,
    /// The `Agent` call that spawned it (ADR-0013 route 1).
    pub spawn_tool_use: Option<ToolUseId>,
    /// The `wf_<runId>` it belongs to (ADR-0013 route 3), for the 503-of-643
    /// workflow subagents that have no `toolUseId` at all.
    pub workflow_run: Option<String>,
}

impl Worker {
    /// A worker first seen at `at`, with no attribution evidence yet.
    pub fn new(id: WorkerId, at: Instant) -> Self {
        Self {
            id,
            kind: AgentType::new(""),
            focus: None,
            running: true,
            attribution: WorkerAttribution::Unknown,
            started: at,
            last_activity: at,
            ops: 0,
            spawn_tool_use: None,
            workflow_run: None,
        }
    }

    /// Records a stronger attribution than the one already held.
    pub(crate) fn upgrade_attribution(&mut self, evidence: WorkerAttribution) {
        if evidence.strength() > self.attribution.strength() {
            self.attribution = evidence;
        }
    }
}

/// How a worker was attached to its thread (ADR-0013, ADR-0030).
///
/// Ranked, because the routes differ in reliability and the UI should be able to
/// say so. `Unknown` is a real state, reached when a worker's records arrived
/// with no parent link at all — it is displayed, not guessed around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerAttribution {
    /// The record carried `agent_id` itself and a session id with it. 100% of
    /// hook payloads inside a subagent, and 61 239/61 239 transcript records.
    RecordAgentId,
    /// `agent-<id>.meta.json`'s `toolUseId` resolved to an `Agent` `tool_use`
    /// (ADR-0013 route 1 — 140/140, but only with a session-wide index).
    SpawnToolUse,
    /// The `wf_<runId>` directory matched the parent `Workflow` call's `runId`
    /// (route 3 — 40/40, and the only link 503 of 643 subagents have).
    WorkflowRun,
    /// The worker id came from the transcript file's own path rather than from a
    /// record field. Reliable in practice, weaker in principle.
    TranscriptFile,
    /// No parent link was available. The worker is in
    /// [`World::unattributed`] and has no thread.
    Unknown,
}

impl WorkerAttribution {
    /// Higher is stronger. Used to avoid downgrading a known link.
    pub fn strength(&self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::TranscriptFile => 1,
            Self::WorkflowRun => 2,
            Self::SpawnToolUse => 3,
            Self::RecordAgentId => 4,
        }
    }

    /// Whether this link came from an authoritative source rather than an
    /// inference. Only [`WorkerAttribution::Unknown`] is not.
    pub fn is_attributed(&self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

/// A worker Polis has seen but cannot attach to any thread.
///
/// > if a worker cannot be attributed to a thread, model that explicitly rather
/// > than guessing
///
/// The alternative — folding it into the main agent — would feed one thread's
/// territory with another's evidence, and PRD §6 territories are exactly the
/// thing that must not absorb evidence they did not earn.
#[derive(Debug, Clone)]
pub struct UnattributedWorker {
    /// The worker.
    pub id: WorkerId,
    /// Its type, when a record named one.
    pub kind: Option<AgentType>,
    /// Where it was last seen acting.
    pub focus: Option<LogicalPath>,
    /// When it was first seen.
    pub first_seen: Instant,
    /// When it was last seen.
    pub last_seen: Instant,
    /// How many records arrived for it with no thread.
    pub records: u32,
    /// The `wf_<runId>` it belongs to, when the record named one. This is what
    /// lets it be adopted later, once the parent `Workflow` call is seen
    /// (ADR-0013 route 3).
    pub workflow_run: Option<String>,
    /// Why the link is missing, in words an operator can act on.
    pub reason: &'static str,
}

/// What a thread is doing (PRD §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadStatus {
    /// Actively calling tools.
    Working,
    /// Blocked on a human. This is what the product is for.
    Waiting,
    /// Alive but quiet.
    Idle,
    /// Finished. Split by verification in [`attention`], because "done,
    /// unverified" is really "needs review".
    Done,
}

impl ThreadStatus {
    /// Status-rail order: waiting first, because unblocking is the primary
    /// decision PRD §1 exists to accelerate.
    pub fn rail_rank(self) -> u8 {
        match self {
            Self::Waiting => 0,
            Self::Working => 1,
            Self::Idle => 2,
            Self::Done => 3,
        }
    }

    /// Whether the thread can still produce work.
    pub fn is_live(self) -> bool {
        !matches!(self, Self::Done)
    }
}

impl fmt::Display for ThreadStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Idle => "idle",
            Self::Done => "done",
        })
    }
}

/// One operation, carrying PRD §10.1's shape channel and §10.2's colour channel
/// as two separate fields — never conflated.
#[derive(Debug, Clone)]
pub struct Operation {
    /// What the call *named*: its first resolved path, or a shell call's
    /// working directory. `None` for a call that named nothing — a `WebSearch`,
    /// an `AskUserQuestion`.
    ///
    /// This is the operation's subject, **not** where it is drawn. A path here
    /// may have no geometry in the current city (a scratchpad, `~/.claude`, a
    /// file added since the last layout run), and on one recorded session 868 of
    /// them did. Where it is drawn is [`Operation::placement`], resolved by
    /// [`place::site_of`].
    pub path: Option<LogicalPath>,
    /// Where it is drawn: rung 1, 2 or 3 of [`place`]'s chain, decided when the
    /// call happened.
    ///
    /// Separate from [`Operation::path`] because "which file is this about" and
    /// "where does this go on the map" are different questions, and collapsing
    /// them is what let 69 % of all tool failures fall off the map: they happen
    /// on shell tools, which name no file.
    pub placement: OpPlacement,
    /// Which tool.
    pub tool: ToolKind,
    /// Shape. Usually [`ToolKind::glyph`], but a shell command that ran the test
    /// suite becomes [`Glyph::ConcentricCircles`] — the "verify" glyph PRD §10.1
    /// lists and no tool owns (see [`verify`]).
    pub glyph: Glyph,
    /// Colour. `Pending` until a result is seen, which is the normal state:
    /// `toolUseResult` is absent on 65% of subagent tool results.
    pub outcome: Outcome,
    /// Which worker did it, or `None` for the main agent.
    pub worker: Option<WorkerId>,
    /// When.
    pub at: Instant,
    /// The cross-channel join key, when the record carried one.
    pub tool_use: Option<ToolUseId>,
}

/// How often, and how recently, a thread has touched one path.
#[derive(Debug, Clone, Copy)]
pub struct VisitStats {
    /// Total touches. This is the thrashing signal and it is never truncated by
    /// [`TRAIL_CAP`].
    pub count: u32,
    /// Of which mutating.
    pub writes: u32,
    /// First touch.
    pub first: Instant,
    /// Most recent touch.
    pub last: Instant,
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// Live per-file state (PRD §5).
#[derive(Debug, Clone, Default)]
pub struct FileState {
    /// Uncommitted diff lines. Drives building height (PRD §7.3).
    ///
    /// Maintained as `lines_added + lines_removed` and updated as each edit
    /// lands, so the city rises while agents work and settles when you merge.
    pub diff_lines: u32,
    /// Last write.
    pub last_touched: Option<Instant>,
    /// Last time a test ran against this file after a change. `None` here is
    /// what makes a finished thread "done, unverified" rather than "done".
    pub last_verified: Option<Instant>,
    /// Which threads have touched it. Four inline covers essentially every real
    /// file; contention needs only two.
    pub touched_by: SmallVec<[ThreadId; 4]>,
    /// Lines added, as reported.
    pub lines_added: u32,
    /// Lines removed, as reported.
    pub lines_removed: u32,
    /// The file's real length, when a `Read` result reported one — PRD §7.3's
    /// footprint input without a `stat()`.
    pub total_lines: Option<u32>,
    /// Set when the file was deleted. Its lot goes to seed (PRD §7.5) rather
    /// than vanishing.
    pub deleted: bool,
    /// Reads seen.
    pub reads: u32,
    /// Writes seen.
    pub writes: u32,
    /// Whether [`FileState::diff_lines`] is exact or approximated (ADR-0004).
    pub diff_precision: DiffPrecision,
}

impl FileState {
    /// Adds a diff delta, keeping [`FileState::diff_lines`] in step and never
    /// upgrading the recorded precision.
    pub(crate) fn add_diff(&mut self, added: u32, removed: u32, precision: DiffPrecision) {
        self.lines_added = self.lines_added.saturating_add(added);
        self.lines_removed = self.lines_removed.saturating_add(removed);
        self.diff_lines = self.lines_added.saturating_add(self.lines_removed);
        // `Unknown` means "nothing has reported yet", so the first report sets
        // the precision and every later one can only degrade it.
        self.diff_precision = if self.diff_precision == DiffPrecision::Unknown {
            precision
        } else {
            self.diff_precision.min(precision)
        };
    }

    /// Registers a toucher without duplicating it.
    pub(crate) fn touch(&mut self, thread: &ThreadId, at: Instant) {
        self.last_touched = Some(at);
        if !self.touched_by.iter().any(|t| t == thread) {
            self.touched_by.push(thread.clone());
        }
    }

    /// Whether a test has run against this file since it was last changed.
    pub fn is_verified(&self) -> bool {
        matches!((self.last_verified, self.last_touched), (Some(v), Some(t)) if v >= t)
    }
}

/// How trustworthy a file's diff-line count is (ADR-0004, PRD §7.3).
///
/// Ordered worst-first so `min` degrades: once any approximate delta has landed
/// on a file, the file's count is approximate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiffPrecision {
    /// Nothing has reported a diff for this file.
    #[default]
    Unknown,
    /// Counted from the edit's own strings, or from an elision marker's line
    /// count. This is what 65% of subagent edits give you, because
    /// `structuredPatch` is absent on them.
    Approximate,
    /// From `structuredPatch` hunks — exact ± counts and real line ranges.
    Exact,
}

// ---------------------------------------------------------------------------
// Territory evidence
// ---------------------------------------------------------------------------

/// One observation feeding territory inference (PRD §6.1).
#[derive(Debug, Clone)]
pub struct Observation {
    /// Which thread made it.
    pub thread: ThreadId,
    /// Which worker, if any.
    pub worker: Option<WorkerId>,
    /// The path observed.
    pub path: LogicalPath,
    /// The tool that produced it; supplies the base weight.
    pub tool: ToolKind,
    /// Whether `path` names a file or a directory. A `Grep` scoped to
    /// `src/auth` claims the directory; a `Read` of `src/auth/token.rs` claims
    /// the file, and PRD §6.2's ancestor is taken over its parent so a single
    /// file cannot become a territory.
    pub scope: PathScope,
    /// When Polis observed it — the hook or OTel clock, never a transcript
    /// timestamp (ADR-0014).
    pub at: Instant,
    /// Weight override, for evidence with no row in PRD §6.1's table.
    ///
    /// `None` uses [`ToolKind::evidence_weight`]. `Some` is used for
    /// `@`-mentions ([`AT_MENTION_WEIGHT`]), which produce no tool call at all
    /// (ADR-0017).
    pub weight: Option<f32>,
}

impl Observation {
    /// The base weight before the ubiquity discount (PRD §6.1).
    pub fn base_weight(&self) -> f32 {
        self.weight.unwrap_or_else(|| self.tool.evidence_weight())
    }

    /// The directory this observation claims, for PRD §6.2's ancestor.
    pub fn claim_path(&self) -> LogicalPath {
        self.scope.claim_of(&self.path)
    }
}

/// Whether an observed path is a file or a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathScope {
    /// A file: `Read`, `Edit`, `Write`, an `@`-mention.
    File,
    /// A directory: `Glob`/`Grep`'s `path`, a shell call's `cwd`.
    Directory,
}

impl PathScope {
    /// The scope a tool's path input usually has.
    ///
    /// `Glob` and `Grep` take a directory to scope the search, and a shell
    /// call's evidence is its `cwd`; everything else names a file. A `Grep`
    /// pointed at a single file is the known exception and costs one level of
    /// ancestor.
    pub fn for_tool(tool: &ToolKind) -> Self {
        match tool {
            ToolKind::Glob | ToolKind::Grep | ToolKind::Bash | ToolKind::PowerShell => {
                Self::Directory
            }
            _ => Self::File,
        }
    }

    /// The directory a path claims under this scope.
    pub fn claim_of(self, path: &LogicalPath) -> LogicalPath {
        match self {
            Self::Directory => path.clone(),
            Self::File => path.parent().unwrap_or_else(LogicalPath::root),
        }
    }
}

// ---------------------------------------------------------------------------
// Ubiquity
// ---------------------------------------------------------------------------

/// The PRD §6.1 ubiquity discount, as the world sees it.
///
/// > Every agent reads the README, `package.json`, and top-level config. […]
/// > scale each observation by `log(N / sessions_that_read_path)`.
///
/// A trait rather than a `polis_repo::corpus::Corpus` field so the world can be
/// driven in a test, or during replay, without a SQLite file — and so a cold
/// start with only the shipped denylist is a first-class configuration rather
/// than a special case.
pub trait Ubiquity: fmt::Debug + Send {
    /// The multiplier for one path, in `(0, 1]`. `1.0` is "no discount".
    fn discount(&self, path: &LogicalPath) -> f32;
}

/// No discount at all. Every observation counts for its full tool weight.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoUbiquity;

impl Ubiquity for NoUbiquity {
    fn discount(&self, _path: &LogicalPath) -> f32 {
        1.0
    }
}

/// The shipped cold-start denylist of common orientation files (PRD §6.1).
#[derive(Debug, Default)]
pub struct DenylistUbiquity(pub Denylist);

impl Ubiquity for DenylistUbiquity {
    fn discount(&self, path: &LogicalPath) -> f32 {
        if self.0.matches(path) {
            polis_repo::corpus::DENYLIST_DISCOUNT
        } else {
            1.0
        }
    }
}

/// The learned TF-IDF discount, from `$XDG_STATE_HOME/polis/corpus.db`.
#[derive(Debug)]
pub struct CorpusUbiquity(pub Corpus);

impl Ubiquity for CorpusUbiquity {
    fn discount(&self, path: &LogicalPath) -> f32 {
        self.0.ubiquity_discount(path)
    }
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// Degraded-channel and drift reporting for the status bar (PRD §4.5, §17).
///
/// > a schema-drift warning in the status bar rather than a crash.
///
/// Every counter here exists because the alternative was to guess. A number that
/// is visible in the status bar is a number an operator can act on; a silently
/// dropped observation is a city that looks healthy and is wrong.
#[derive(Debug, Clone, Default)]
pub struct Health {
    /// Events dropped, per channel, since start.
    pub dropped: BTreeMap<String, u64>,
    /// Channels running degraded, with the reason.
    pub degraded: BTreeMap<String, String>,
    /// Unrecognised events, fields and record types seen.
    pub drift: u64,
    /// True when the beta traces channel produced no spans and every tool call
    /// is therefore attributed to the main agent. Displayed, never guessed
    /// around (ADR-0006).
    pub subagent_attribution_degraded: bool,
    /// Events handed to [`World::apply`].
    pub events_applied: u64,
    /// Events understood but not modelled — startup traffic, plugin loads.
    pub events_ignored: u64,
    /// Workers parked in [`World::unattributed`].
    pub unattributed_workers: u64,
    /// Paths that resolved to no worktree, and therefore to no building.
    pub unmapped_paths: u64,
    /// Observations whose path has no geometry in the current city, so they fed
    /// convergence but dropped no kernel.
    pub unplaced_observations: u64,
    /// `@`-mentions seen. A known blind spot in PRD §6.1 (ADR-0017); the OTel
    /// `at_mention` event carries no path, so only the transcript's `attachment`
    /// records can be turned into observations.
    pub at_mentions: u64,
    /// `@`-mentions that carried no usable path and therefore contributed
    /// nothing.
    pub at_mentions_without_path: u64,
    /// Holes in `event.sequence` — telemetry lost between Claude Code and Polis.
    pub sequence_gaps: u64,
    /// `claude_code.tool` spans seen. Zero of these plus a non-zero
    /// [`Health::otel_tool_results_without_worker`] is exactly the ADR-0006
    /// degraded mode.
    pub otel_tool_spans: u64,
    /// Logs-channel tool results with no worker, which the logs channel can
    /// never supply (`agent_id` is on the span only).
    pub otel_tool_results_without_worker: u64,
    /// Contention hits that had to degrade to file level for want of a line
    /// range (ADR-0004).
    pub contention_without_line_ranges: u64,
    /// Working directories adopted as additional checkouts (PRD §7.6).
    pub adopted_worktrees: u64,
    /// Every operation, by the rung of [`place`]'s chain it landed on when it
    /// happened.
    ///
    /// The map used to draw only operations that resolved to a building, and
    /// this counter is what makes the rest countable instead of invisible.
    pub ops: PlacementCensus,
    /// The subset of [`Health::ops`] that **failed**.
    ///
    /// Counted separately and deliberately: a failed shell command is the most
    /// decision-changing mark on the map (PRD §17), and 69 % of real failures
    /// are on tools that carry no path. If this ever starts filling
    /// [`PlacementCensus::rail`], failures are going unseen again.
    pub ops_failed: PlacementCensus,
    /// Results that arrived after their operation had already been pushed out
    /// of [`OPS_CAP`], so the outcome had nowhere to land.
    ///
    /// These are marks that stay neutral for ever. Non-zero means the cap is
    /// too small for the traffic, and it is measured rather than assumed.
    pub ops_settled_after_eviction: u64,
}

impl Health {
    /// Clears the per-run counters, keeping the channel status.
    ///
    /// Used by [`World::reset`] so a replay seek does not accumulate counts from
    /// the events it is about to apply again.
    pub fn reset_counters(&mut self) {
        let degraded = std::mem::take(&mut self.degraded);
        let dropped = std::mem::take(&mut self.dropped);
        *self = Self {
            dropped,
            degraded,
            ..Self::default()
        };
    }

    /// True when anything is wrong enough to say so in the status bar.
    pub fn is_degraded(&self) -> bool {
        !self.degraded.is_empty()
            || self.subagent_attribution_degraded
            || self.dropped.values().any(|n| *n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    #[test]
    fn a_file_scope_claims_its_directory_so_one_file_is_not_a_territory() {
        // PRD §6.2 gates on `depth(A) >= 2`; without folding a file to its
        // parent, a single read of `src/auth/token.rs` would satisfy it alone.
        assert_eq!(
            PathScope::File.claim_of(&lp("src/auth/token.rs")).as_str(),
            "src/auth"
        );
        assert_eq!(
            PathScope::Directory.claim_of(&lp("src/auth")).as_str(),
            "src/auth"
        );
        assert!(PathScope::File.claim_of(&lp("README.md")).is_root());
    }

    #[test]
    fn scope_follows_the_tool_and_both_shells_count() {
        assert_eq!(PathScope::for_tool(&ToolKind::Grep), PathScope::Directory);
        assert_eq!(PathScope::for_tool(&ToolKind::Glob), PathScope::Directory);
        // ADR-0032: never test for Bash alone.
        assert_eq!(PathScope::for_tool(&ToolKind::Bash), PathScope::Directory);
        assert_eq!(
            PathScope::for_tool(&ToolKind::PowerShell),
            PathScope::Directory
        );
        assert_eq!(PathScope::for_tool(&ToolKind::Read), PathScope::File);
        assert_eq!(PathScope::for_tool(&ToolKind::Edit), PathScope::File);
    }

    #[test]
    fn diff_precision_degrades_and_never_upgrades() {
        let mut f = FileState::default();
        f.add_diff(10, 2, DiffPrecision::Exact);
        assert_eq!(f.diff_lines, 12);
        assert_eq!(f.diff_precision, DiffPrecision::Exact);
        // A subagent edit with no `structuredPatch` lands next; the file's count
        // is now approximate and must say so.
        f.add_diff(3, 0, DiffPrecision::Approximate);
        assert_eq!(f.diff_lines, 15);
        assert_eq!(f.diff_precision, DiffPrecision::Approximate);
        f.add_diff(1, 1, DiffPrecision::Exact);
        assert_eq!(
            f.diff_precision,
            DiffPrecision::Approximate,
            "precision must not be laundered back to exact"
        );
    }

    #[test]
    fn attribution_strength_is_ordered_and_unknown_is_not_attributed() {
        assert!(
            WorkerAttribution::RecordAgentId.strength()
                > WorkerAttribution::SpawnToolUse.strength()
        );
        assert!(
            WorkerAttribution::SpawnToolUse.strength() > WorkerAttribution::WorkflowRun.strength()
        );
        assert!(
            WorkerAttribution::WorkflowRun.strength()
                > WorkerAttribution::TranscriptFile.strength()
        );
        assert!(!WorkerAttribution::Unknown.is_attributed());
        assert!(WorkerAttribution::TranscriptFile.is_attributed());

        let mut w = Worker::new(WorkerId::new("a1"), Instant::now());
        w.upgrade_attribution(WorkerAttribution::TranscriptFile);
        w.upgrade_attribution(WorkerAttribution::RecordAgentId);
        w.upgrade_attribution(WorkerAttribution::TranscriptFile);
        assert_eq!(w.attribution, WorkerAttribution::RecordAgentId);
    }

    #[test]
    fn the_trail_is_capped_but_revisit_counts_are_not() {
        // PRD §12: "the same building revisited six times" is the signal, and it
        // has to survive the trail cap.
        let mut t = Thread::new(
            ThreadId::of_session(SessionId::new("s")),
            SessionId::new("s"),
            Instant::now(),
        );
        let hot = lp("src/auth/token.rs");
        for i in 0..(TRAIL_CAP * 2) {
            let at = Instant::now();
            t.step_trail(hot.clone(), at);
            t.step_trail(lp(&format!("src/other/f{i}.rs")), at);
        }
        assert!(t.trail.len() <= TRAIL_CAP);
        assert_eq!(t.revisits(&hot), u32::try_from(TRAIL_CAP * 2).unwrap());
        assert_eq!(t.most_revisited().map(|(p, _)| p.clone()), Some(hot));
    }

    #[test]
    fn a_repeated_step_refreshes_rather_than_stacking() {
        let mut t = Thread::new(
            ThreadId::of_session(SessionId::new("s")),
            SessionId::new("s"),
            Instant::now(),
        );
        let p = lp("src/a.rs");
        let t0 = Instant::now();
        t.step_trail(p.clone(), t0);
        t.step_trail(p.clone(), t0 + Duration::from_secs(1));
        assert_eq!(t.trail.len(), 1);
        assert_eq!(t.revisits(&p), 2);
        assert_eq!(t.trail[0].1, t0 + Duration::from_secs(1));
    }

    #[test]
    fn trails_fade_on_tick() {
        let mut t = Thread::new(
            ThreadId::of_session(SessionId::new("s")),
            SessionId::new("s"),
            Instant::now(),
        );
        let t0 = Instant::now();
        t.step_trail(lp("a.rs"), t0);
        t.step_trail(lp("b.rs"), t0 + Duration::from_secs(1));
        t.fade_trail(t0 + TRAIL_TTL / 2);
        assert_eq!(t.trail.len(), 2, "nothing fades before the TTL");
        t.fade_trail(t0 + TRAIL_TTL + Duration::from_millis(500));
        assert_eq!(t.trail.len(), 1, "the older step faded first");
        t.fade_trail(t0 + TRAIL_TTL * 2);
        assert!(t.trail.is_empty());
        // The revisit counts survive the fade — the trail is a view, not the
        // record.
        assert_eq!(t.visits.len(), 2);
    }

    #[test]
    fn status_rail_puts_waiting_first() {
        // PRD §1: the primary decision is *unblock*.
        assert!(ThreadStatus::Waiting.rail_rank() < ThreadStatus::Working.rail_rank());
        assert!(ThreadStatus::Working.rail_rank() < ThreadStatus::Idle.rail_rank());
        assert!(ThreadStatus::Idle.rail_rank() < ThreadStatus::Done.rail_rank());
        assert!(!ThreadStatus::Done.is_live());
        assert_eq!(ThreadStatus::Waiting.to_string(), "waiting");
    }

    #[test]
    fn ubiquity_sources_agree_on_their_contract() {
        let readme = lp("README.md");
        let deep = lp("src/auth/token.rs");
        assert!((NoUbiquity.discount(&readme) - 1.0).abs() < f32::EPSILON);
        let deny = DenylistUbiquity::default();
        assert!(
            deny.discount(&readme) < deny.discount(&deep),
            "the shipped denylist must discount orientation files"
        );
        assert!(deny.discount(&readme) > 0.0, "a discount is never zero");
    }
}
