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
//! | [`shell`] | §6.1 — the path evidence inside a shell command line |
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
pub mod shell;
pub mod snapshot;
pub mod territory;
pub mod verify;

mod apply;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::time::{Duration, Instant};

use polis_events::{
    AgentType, Event, Glyph, LogicalPath, Outcome, PathMapper, Payload, SessionId, ThreadId,
    ToolKind, ToolUseId, WorkerId, WorktreeId, IDENTITY_SLOTS,
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

/// How many of the agent's own call summaries a thread keeps ([`Intent`]).
///
/// Twenty. The question this ring exists to answer is *"what is this thread
/// working on **now**"*, and twenty calls is roughly the last few minutes of a
/// working thread — long enough that one `Read` in the middle of an edit does
/// not become the whole story, short enough that a summary written from it is
/// about the present rather than the session. [`OPS_CAP`] is 128 because the
/// map draws a decaying trail; nothing draws this.
pub const INTENT_CAP: usize = 20;

/// The longest single summary kept, in characters.
///
/// Every one of these is written by an agent asked for "a clear, concise
/// description", and the ones that come back long are long because a command
/// was pasted into them. Cutting at 160 keeps the sentence and loses the paste.
pub const INTENT_TEXT_MAX: usize = 160;

/// How long a thread may be silent before [`ThreadStatus::Working`] decays to
/// [`ThreadStatus::Idle`].
///
/// Idle is "alive but quiet", not "finished": only an explicit stop signal
/// produces [`ThreadStatus::Done`].
///
/// # Silence is the test only while nothing is in flight
///
/// A `Bash` or `PowerShell` call writes **nothing at all** between its
/// `tool_use` block and its `tool_result`: the transcript is append-only and the
/// tool's whole output arrives in one record at the end. A session running
/// `cargo clippy --workspace` is therefore byte-for-byte as quiet as a session
/// whose operator walked away, and sixty seconds of nothing is not evidence of
/// idleness on its own.
///
/// Measured over this machine's `~/.claude/projects` — 1 374 transcripts,
/// 65 652 settled tool calls, the two tools that ask the operator excluded
/// because those are the human's own latency ([`attention::asks_the_operator`]):
/// the median call is 0.93 s, but **p99 is 120 s — twice this constant — and
/// 3.27% of all calls outlive it**. That tail is not noise; it is the long
/// build, the full test run and the dispatched subagent fleet, which is
/// precisely the work an operator most needs to see is still running.
///
/// [`IN_FLIGHT_MAX`] is the other half of the test, and [`World::tick`] applies
/// them together: a thread goes `Idle` when it has been silent for this long
/// **and** owns no unsettled tool call — where "owns" is what the transcript
/// channel saw, since that is the only channel that fills the table.
pub const IDLE_AFTER: Duration = Duration::from_secs(60);

/// How long an unsettled tool call still counts as work in progress.
///
/// [`IDLE_AFTER`] asks *"has this thread been silent?"*. This asks *"is it
/// silent because it is waiting on something it started?"* — [`World::tick`]
/// holds a thread [`ThreadStatus::Working`] while it owns a `tool_use` whose
/// `tool_result` has not arrived and which started inside this window.
///
/// A ceiling is needed because the `tool_use` is not proof on its own. A session
/// killed mid-`Bash`, a transcript truncated mid-turn, a `/clear` on top of a
/// running call: each leaves a [`PendingCall`] that will never be settled, and
/// without a ceiling that one entry paints a live-looking dot on the map for the
/// rest of the thread's life.
///
/// # Where ten minutes comes from
///
/// Same corpus as [`IDLE_AFTER`] above. Each row is what that cap would leave
/// unprotected — calls long enough to still flip their thread to `Idle`
/// mid-call:
///
/// | cap | calls that still go Idle mid-call |
/// |---:|---|
/// | 2 min | 787 (1.199%) |
/// | 3 min | 209 (0.318%) |
/// | 5 min | 123 (0.187%) |
/// | **10 min** | **44 (0.067%)** |
/// | 15 min | 4 (0.006%) |
/// | 30 min | 0 |
///
/// Too low and the defect returns for exactly the calls that motivated it: of
/// the 44 machine calls past ten minutes, 28 are `Bash`, 7 are `Agent` and 6 are
/// `TaskOutput` — long builds and orchestrated subagent fleets, never a quick
/// read. Too high and the failure inverts: a session that died mid-call reads
/// `Working` for however long this is, and at 30 minutes that equals
/// [`THREAD_RETIRE_AFTER`], so the lie would run right up to the moment the
/// thread is retired and no clock in the world would ever correct it. Ten
/// minutes bounds the lie at a third of a thread's life while covering
/// 99.933% of real calls.
///
/// **Asserted at the top end, not swept.** The knee of that table is at 15
/// minutes, not 10 — 15 removes 40 of the remaining 44 — and 10 was chosen for
/// the margin against [`THREAD_RETIRE_AFTER`] rather than measured against a
/// cost. One machine, one operator, one corpus, whose longest machine call is
/// 1 724 s. The 44 calls above the cap are still drawn `Idle` for their tail.
///
/// **Channel D only, and the gap is real.** The table this reads is filled in
/// exactly one place — `apply::assistant`'s `tool_use` loop — so a deployment
/// fed by hooks or by OTel alone gets nothing from this constant, and its long
/// build still reads `Idle` for the whole run and corrects itself only at the
/// completion edge. `PreToolUse` is the hook that would close it; it is not
/// wired to `pending` today.
pub const IN_FLIGHT_MAX: Duration = Duration::from_mins(10);

/// How long a background job may run before Polis stops believing in it.
///
/// About 5% of launched jobs (17 of 357 measured here) never produce the
/// `<task-notification>` that would close them — the session was killed, the
/// terminal closed, the shell reaped. Without a ceiling those pin a thread
/// [`ThreadStatus::Parked`] forever, which is quieter and therefore worse than
/// the `WAITING` it replaced.
///
/// Twelve hours, not the hour a reap timer would suggest, because the ceiling
/// has to sit above real jobs rather than through them: measured
/// launch-to-notification on this machine is p90 22 min but **p99 10.4 h**, with
/// a longest legitimate job of 12.0 h.
pub const BACKGROUND_MAX: Duration = Duration::from_hours(12);

/// How long a quiet thread stays in the world before [`World::tick`] retires it.
///
/// **Deliberately the same 30 minutes as `polis_ingest::live::LIVE_WINDOW`**,
/// which is the point at which the ingest side stops paying to follow a
/// session. Two different numbers here would mean either a thread that outlives
/// every channel that could ever update it again, or one that vanishes off the
/// map while its transcript is still being read.
///
/// It has to exist at all because *a session never ends on disk*: the JSONL
/// format has no session-end record (`docs/verified/jsonl-schema.md` enumerates
/// all 19 types and none of them closes a session), so without a retirement rule
/// a Polis left open on the operator's second monitor accumulates one [`Thread`]
/// per session for as long as it runs — each holding a trail, an operation ring,
/// a visit map and a territory. PRD §13.1 budgets an idle cost, and an
/// unbounded thread map is how that budget is lost slowly enough not to notice.
pub const THREAD_RETIRE_AFTER: Duration = Duration::from_mins(30);

/// The ceiling on how long a standing attention mark may hold a silent thread.
///
/// `World::retire_threads` will not take a thread the operator still has to
/// act on, and only `Done { verified: true }` marks decay
/// ([`attention::AttentionKind::decays`]) — so a *needs decision* Polis never
/// saw answered, or a *done, unverified*, pinned its thread to the rail
/// **forever**. That is not a queue that stays useful; it is how a rail meant to
/// be read in under a second accumulates every session of the day, and a
/// `Waiting` thread is exempt from [`IDLE_AFTER`] and from [`MAX_THREADS`]
/// eviction as well, so nothing else could ever clear it.
///
/// Eight retirement windows. Past four hours of total silence the mark cannot be
/// resolved by anything: `polis_ingest::live::LIVE_WINDOW` stopped following the
/// session seven windows ago, so no channel can ever update it, and what is left
/// is a session that ended rather than a decision that is waiting. The operator
/// who wants one gone sooner has the rail's `✕` ([`World::dismiss_thread`]).
pub const MARK_HOLD_MAX: Duration = Duration::from_hours(4);

/// How long an unattributable worker is remembered before it is dropped.
///
/// Longer than [`THREAD_RETIRE_AFTER`] on purpose: an unattributed worker is
/// waiting to be *adopted* (ADR-0013 route 3 adopts on the parent `Workflow`
/// call, which can arrive long after the journal that named the worker), and
/// forgetting it early would turn a recoverable link into a permanent one.
pub const UNATTRIBUTED_TTL: Duration = Duration::from_mins(60);

/// Most contention marks the attention layer carries at once.
///
/// PRD §11.2's other two states are bounded by the number of threads;
/// contention is bounded by the number of contended **files**, which under
/// PRD §16's synthetic load was 1 600 after [`contention::MAX_CLAIMS_PER_PATH`]
/// had already cut the pairings down from 480 000. Thirty-two red links is well
/// past the point where a thirty-third changes a decision (PRD §17), and the
/// overflow is counted in [`Health::contention_over_cap`] rather than dropped
/// silently.
pub const MAX_CONTENTION_MARKS: usize = 32;

/// Whether two workers of the **same** agent writing one file raise contention.
///
/// `false`, and that is a correction rather than a preference. PRD §11.3 defines
/// contention as *"a relation between two threads, not a property of one"*, and
/// two workers an orchestrator dispatched onto the same file are one thread
/// doing coordinated work — the sequencing is the orchestrator's, and no human
/// decision is pending on it.
///
/// It was briefly enabled after a gate correctly found that worker-vs-worker
/// collisions inside one session could never fire. The fix went too far: on the
/// operator's own live map it produced 32 of 36 attention items, all reading
/// "same file · two workers of one agent", every one outranking the amber pins
/// by §11.1's ordering — burying the one state PRD §11.2 calls *"the primary
/// state; it is what the product is for"*. That is PRD §17's named failure mode
/// exactly: a signal that makes the operator feel informed while telling them
/// nothing actionable.
///
/// Cross-thread contention is untouched and still fires.
pub const CONTENTION_WITHIN_THREAD: bool = false;

/// Hard ceiling on live threads, whatever the clock says.
///
/// The retirement rule above is the one that normally binds; this is the guard
/// against the case it cannot see — a burst of sessions inside one retirement
/// window. PRD §16's synthetic load is *"100 threads × 400 subagents"*, so the
/// ceiling sits well above the load the product is specified to survive and only
/// ever evicts the stalest non-waiting thread.
pub const MAX_THREADS: usize = 256;

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
// Identity hue — who owns which of PRD §11.4's twelve colours
// ---------------------------------------------------------------------------

/// Which of the [`IDENTITY_SLOTS`] hues have been handed out, and never to two
/// threads at once.
///
/// # What this replaces, and why
///
/// The hue used to be `polis_render::live::thread_slot` — FNV over the id,
/// modulo twelve, computed independently by five draw sites. Its own
/// doc-comment accepted collisions as the price of stability, and with nine
/// threads on screen about three of the thirty-six pairs shared a colour. The
/// operator rejected the trade outright: *"the same color is super bad, can you
/// make sure the colors have to be different?"*. A hash cannot promise that, at
/// any ring size — twenty slots would still collide, and would cost more than
/// the collision does (see `polis_render::live::THREAD_HUES`'s ΔE00 table). The
/// only structure that *can* promise it is an owner that hands each slot out
/// once, which is this.
///
/// [`ThreadId::hue_preference`] is still the first slot tried, so a world with a
/// handful of threads paints exactly the colours it painted before.
///
/// # A slot is never released, and that is the determinism argument
///
/// A thread leaving the world does not give its colour back. Nothing here is
/// keyed on [`World::threads`] at all: the ring records which *id* took which
/// slot and keeps that for the life of the world, so a thread retired for
/// silence and then heard from again gets the colour it had before.
///
/// That looks wasteful and it is the whole reason this is sound. Retirement is
/// driven by [`World::tick`], and **tick cadence differs between the two
/// renderers on the same recording**: `replay::ReplayDriver::advance` ticks once
/// per call and the window calls it many times, so [`World::retire_threads`]
/// fires repeatedly mid-run; `run_to_end` applies the whole schedule and ticks
/// exactly once, so it fires at the end or not at all. Two things follow, and a
/// ring that recycled slots would get both wrong:
///
/// * whether a slot is **free** when the next thread is created — so a recycling
///   ring would hand the thirteenth thread a reused colour in the window and the
///   bare fallback in a recorded GIF;
/// * whether a thread is created **twice** — a session that goes quiet past
///   [`THREAD_RETIRE_AFTER`] and then speaks again is retired and rebuilt by the
///   window, and is not retired at all by `run_to_end`, so it reaches
///   [`HueRing::assign`] a different number of times in the two.
///
/// The second is the one that is easy to miss, and remembering the owner is what
/// answers it: a second `assign` for an id the ring has already placed is a
/// lookup, not a claim, so it consumes nothing and returns the same slot in both
/// drivers. `polis_app::palette` names the failure both of these produce in as
/// many words — a colour that meant one thread in the window and another in a
/// recorded GIF *"would be worse than no colour at all"*.
///
/// What is left is an assignment that depends only on the **set of distinct
/// thread ids and the order they were first seen**, which is the event order:
/// identical in both drivers, and identical across a [`World::reset`]-and-
/// re-apply seek.
///
/// # The price, stated plainly
///
/// A world degrades to the bare preference after **twelve distinct threads have
/// ever been seen in it**, not twelve concurrently. The guarantee is therefore:
/// *the first twelve threads of a world are mutually distinct; past that, colour
/// degrades to the bare preference and the rail's name carries identity
/// (PRD §11.4)*. Past twelve the thirteenth thread is not merely *likely* to
/// collide, it is **certain** to, because every slot is taken.
///
/// **This is reachable on the operator's own machine, and it is worth being
/// blunt about how reachable.** Measured over their `~/.claude/projects` — 193
/// main-session transcripts, subagent sidecars excluded because a subagent
/// carries its parent's session id (ADR-0030) and is not a thread:
///
/// | question | answer |
/// |---|---:|
/// | peak threads live at once, counting a session live until its last record + [`THREAD_RETIRE_AFTER`] | **13** |
/// | distinct sessions started per day, median | 15 |
/// | days with more than twelve distinct sessions | 8 of 15 (53 %) |
/// | hours from a Polis start to its twelfth distinct session, median | 17.0 (p10 2.4) |
///
/// So at this fleet's busiest moment the ring is **one slot short**, and a Polis
/// left open overnight exhausts it by accretion in well under a day even with
/// two threads on screen. Read that as the honest ceiling on the promise: it
/// removes the collisions the operator actually complained about — three shared
/// pairs among nine live threads — and does not remove them all. What it also
/// does is make the remainder *countable*, in [`Health::identity_hues_exhausted`]
/// and in the status bar, rather than a silent return of the reported bug. The
/// operator's remedy is a restart, which is a [`World::reset`], which clears the
/// ring.
///
/// Widening the ring is not the fix, and that is measured too: see
/// `polis_render::live::THREAD_HUES`, whose ΔE00 table shows twenty slots taking
/// the worst pair from 9.80 to 5.4, below what the cloud band already struggles
/// with. The fix, if the fleet keeps growing, is a second channel on the rail
/// row and not a thirteenth hue.
///
/// Both numbers above came from a script over the operator's real sessions that
/// is **not in the tree**, so they cannot currently be re-taken from a test.
/// Treat them the way `territory::CLOUD_CAP`'s doc asks its own numbers to be
/// treated: one machine, one fortnight, one operator.
///
/// The memory is bounded at twelve ids by construction: once every slot is
/// taken nothing more is ever recorded, so this is not a table that grows with
/// the number of sessions the machine has ever run.
#[derive(Debug, Default)]
struct HueRing {
    /// Slot → the thread that took it, ever. Never cleared except by
    /// [`World::reset`].
    held: [Option<ThreadId>; IDENTITY_SLOTS as usize],
}

impl HueRing {
    /// The slot for `id` — the one it already had, or a fresh claim, or `None`
    /// when all twelve are gone.
    ///
    /// The lookup walks `held` in **index** order and the probe is
    /// `(pref + k) % IDENTITY_SLOTS` for `k` in `0..`, both fixed index walks —
    /// never a scan of a map, so no iteration order can reach the answer
    /// (PRD §7.4, and the `BTreeMap` rule this module opens with). The probe
    /// starts at the thread's own preference so displacement is as local as it
    /// can be: a thread only moves off its preferred hue when another thread is
    /// already holding it.
    fn assign(&mut self, id: &ThreadId) -> Option<u8> {
        if let Some(slot) = self.held.iter().position(|h| h.as_ref() == Some(id)) {
            #[allow(clippy::cast_possible_truncation)] // position over 12 slots
            return Some(slot as u8);
        }
        let pref = id.hue_preference();
        for k in 0..IDENTITY_SLOTS {
            let slot = (pref + k) % IDENTITY_SLOTS;
            if self.held[slot as usize].is_none() {
                self.held[slot as usize] = Some(id.clone());
                return Some(slot);
            }
        }
        None
    }
}

/// Which cloud layer each thread's field is drawn on, and never two threads on
/// one layer.
///
/// # Why this is not the hue
///
/// The renderer groups cloud kernels by a small integer — [`Thread::layer`] —
/// and every surface that paints a cloud looks the thread's colour up by it.
/// The obvious integer is [`Thread::tint`], and it is the wrong one twice over.
///
/// It is not **unique**: [`HueRing`] hands out twelve slots and then degrades to
/// the bare preference, so past twelve threads two of them share a tint, and two
/// territories sharing a layer are *summed* — PRD §6.4's contention signal reads
/// two threads in one place as one thread working hard, which is the one
/// distinction the layer exists to make.
///
/// And it is not what was there before: what the two producers actually passed
/// was the thread's **position in the visible list**, which
/// [`territory::select_clouds`] sorts by `last_activity`. Two agents working at
/// once swap that position every time either one runs a tool. The layer id is
/// what the tween matches its two sides on and what the tint table is indexed
/// by, so a swap made `polis_render::live::CloudTween` ease each cloud's shape
/// toward the *other's* and repaint both in the other's hue — the operator's
/// report was *"the clouds contract and rebuild a lot, colours change a lot"*.
/// Ranking still decides which territories survive PRD §10.4's cap and which is
/// painted over which; it no longer decides **who anything is**.
///
/// # Never released, for the reason the hue ring is not
///
/// A monotonic counter alone would be wrong, and [`HueRing`]'s doc has the
/// argument in full: a thread retired for silence and heard from again is
/// created **twice** in the window and once under `run_to_end`, so a counter
/// would give it a second id in one driver and not the other, and the two
/// renderers would disagree about a recording they were both handed. Recording
/// the owner makes a second `assign` a lookup, so the id depends only on the set
/// of thread ids and the order they were first seen — the event order, in both
/// drivers and across a [`World::reset`]-and-re-apply seek.
///
/// The table therefore grows by one `(ThreadId, u16)` per distinct thread the
/// world has ever seen, where [`HueRing`] stops at twelve. That is the price of
/// an id that stays unique past the twelfth thread, and it is a few dozen bytes
/// a session against a bug the operator can see.
#[derive(Debug, Default)]
struct LayerRing {
    /// The thread that took each layer, ever. Never cleared except by
    /// [`World::reset`].
    held: BTreeMap<ThreadId, u16>,
    /// The next unclaimed layer.
    next: u16,
}

impl LayerRing {
    /// The layer for `id` — the one it already had, or a fresh claim.
    ///
    /// Saturates at [`u16::MAX`], so a world that has seen 65 535 distinct
    /// threads puts every one after that on the last layer and sums them. That
    /// is 65 523 threads past the point [`HueRing`] gives up, and both would be
    /// long past PRD §16's hundred.
    fn assign(&mut self, id: &ThreadId) -> u16 {
        if let Some(layer) = self.held.get(id) {
            return *layer;
        }
        let layer = self.next;
        self.next = self.next.saturating_add(1);
        self.held.insert(id.clone(), layer);
        layer
    }
}

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
    /// §11.3's early warning: pairs of threads whose clouds overlap, worst
    /// first.
    ///
    /// Deliberately **not** in [`World::attention`]. See
    /// [`contention::TerritoryOverlap`] for why a signal that fires before
    /// anything is destroyed must not compete for the eye with the one that
    /// fires while it is.
    pub overlaps: Vec<contention::TerritoryOverlap>,
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
    /// Whether the quiet-thread clock runs. Off for a replay.
    ///
    /// Retirement exists to bound a **live** world — `MAX_THREADS` pressure, and
    /// forgetting sessions that ended hours ago. A replay has neither problem:
    /// the driver owns the whole timeline and the operator scrubs it.
    ///
    /// Leaving it on made replay non-deterministic, because it makes the world a
    /// function of the *tick schedule* rather than of the events. Seeking to the
    /// midpoint ticks there and retires a quiet thread; the thread's next event
    /// then rebuilds it from zero, so the same moment reached by seeking and by
    /// playing straight through disagreed on tool counts and worker attribution.
    /// `real_corpus`'s seek-determinism test caught it once ADR-0102 stopped
    /// every finished turn pinning its thread for four hours and threads could
    /// reach the clock at all.
    retire_quiet_threads: bool,
    /// Highest `observed` seen, so `tick` has a floor even before the first one.
    now: Instant,
    /// Bumped whenever [`Self::layout`] is replaced.
    layout_generation: u64,
    /// Bumped whenever [`Self::files`] changes.
    files_generation: u64,
    /// Who owns which identity hue (PRD §11.4). Private, because the only way
    /// to get a slot is to be created through [`World::thread_entry`].
    hues: HueRing,
    /// Who owns which cloud layer. Private for the same reason, and assigned in
    /// the same breath.
    layers: LayerRing,
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
            overlaps: Vec::new(),
            health,
            unattributed: BTreeMap::new(),
            mapper,
            ubiquity: Box::new(NoUbiquity),
            pending: BTreeMap::new(),
            workflow_runs: BTreeMap::new(),
            spawns: BTreeMap::new(),
            adopt_worktrees: true,
            retire_quiet_threads: true,
            now: Instant::now(),
            layout_generation: 0,
            files_generation: 0,
            hues: HueRing::default(),
            layers: LayerRing::default(),
        }
    }

    /// A world with no repository index, for replaying a session over a city
    /// that was generated elsewhere (PRD §15 M2).
    ///
    /// The session's own `cwd` becomes the primary checkout the first time a
    /// record carries one, which is what makes `polis replay <transcript>` work
    /// with nothing configured.
    pub fn for_replay(layout: CityLayout) -> Self {
        let mut world = Self::new(RepoTree::default(), layout);
        world.retire_quiet_threads = false;
        world
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
        self.health.claim_paths_evicted = self.claims.paths_evicted();
        // A thread that owns an unsettled `tool_use` is working, whatever the
        // transcript says — see `IN_FLIGHT_MAX`. Read, never `retain`:
        // dropping the stale entries here would make a late `tool_result` miss
        // the diff and verification accounting `apply::settle` does with them,
        // so the ceiling is applied to the *question* and the table is left
        // alone. `pending` and `threads` are disjoint fields, so the immutable
        // borrow coexists with `values_mut()` below without a clone.
        let in_flight: BTreeSet<&ThreadId> = self
            .pending
            .values()
            .filter(|c| now.saturating_duration_since(c.started) <= IN_FLIGHT_MAX)
            .map(|c| &c.thread)
            .collect();
        for thread in self.threads.values_mut() {
            thread.territory.decay(now);
            thread.fade_trail(now);
            // Silence only ever demotes a thread that claimed to be *working*.
            // `Waiting`, `Interrupted`, `Parked` and `Ready` each rest on a
            // record that was actually seen, and a clock must not overrule
            // evidence — a parked `cargo test` is legitimately silent for
            // minutes (measured p90 22 min, p99 10.4 h), and an interrupt is
            // silent until the operator types.
            if thread.status == ThreadStatus::Working
                && now.saturating_duration_since(thread.last_activity) > IDLE_AFTER
                && !in_flight.contains(&thread.id)
            {
                thread.status = ThreadStatus::Idle;
            }
            // Reap background jobs whose notification never came — measured at
            // 17 of 357, about 5%. Without this a thread parks forever, which is
            // worse than the bug being fixed because `Parked` is quiet.
            //
            // The ceiling is elapsed time, and it is deliberately generous:
            // launch-to-notification runs to p99 10.4 h on this machine, so a
            // tighter clock would relabel legitimately running overnight jobs.
            let before = thread.background.len();
            thread.background.retain(|b| b.age(now) <= BACKGROUND_MAX);
            if thread.background.len() != before && thread.background.is_empty() {
                thread.status = match thread.status {
                    ThreadStatus::Parked => ThreadStatus::Ready,
                    other => other,
                };
            }
        }
        self.refresh_contention(now);
        self.refresh_overlaps(now);
        attention::expire(&mut self.attention, now);
        // After the marks have been expired, never before: a thread is retired
        // only when nothing on the attention layer still points at it, so the
        // rule below reads a list that is already current.
        if self.retire_quiet_threads {
            self.retire_threads(now);
        }
        self.expire_unattributed(now);
        attention::sort(&mut self.attention);
    }

    /// Retires threads that have gone quiet, and enforces [`MAX_THREADS`].
    ///
    /// # The clock never takes a thread the operator still has to act on
    ///
    /// A thread is retired for silence only when **nothing on the attention
    /// layer still points at it**. That is one rule covering both states PRD
    /// §11.2 says persist:
    ///
    /// * *needs decision* — a thread blocked on a human, which may sit there for
    ///   hours. That *is* the product (PRD §1: *"get to the thread that is
    ///   waiting on a human"*).
    /// * *done, unverified* — *"really `needs review`"*, and *"the second most
    ///   important thing on the map"*. It says code was changed and never
    ///   tested, which is actionable however old it is.
    ///
    /// *Done, verified* decays to nothing inside a minute (PRD §11.2's 20 s plus
    /// its ramp), so a thread that finished cleanly retires on the ordinary
    /// silence rule and one that finished dirty does not.
    ///
    /// Two things override it, because unbounded growth is the one outcome worse
    /// than losing a mark: [`MAX_THREADS`], and [`MARK_HOLD_MAX`] — past which a
    /// mark is holding a thread no channel can still reach, so it is history
    /// rather than a queue item. [`World::dismiss_thread`] is the operator's own
    /// override, for the one they can see is finished before either fires.
    ///
    /// # Every way a session ends looks the same, and that is honest
    ///
    /// A session that ended, one that crashed and one whose transcript was
    /// deleted mid-tail are **indistinguishable on disk** — the JSONL format has
    /// no session-end record at all — so all three leave by this one door on
    /// silence alone rather than being told apart by invented evidence. None of
    /// them can leave anything behind: see [`World::forget_thread`].
    fn retire_threads(&mut self, now: Instant) {
        let stale: Vec<ThreadId> = self
            .threads
            .values()
            .filter(|t| {
                let quiet = now.saturating_duration_since(t.last_activity);
                quiet > THREAD_RETIRE_AFTER
                    && (quiet > MARK_HOLD_MAX
                        || !self.attention.iter().any(|m| m.kind.mentions_thread(&t.id)))
            })
            .map(|t| t.id.clone())
            .collect();
        for id in stale {
            self.forget_thread(&id);
            self.health.threads_retired = self.health.threads_retired.saturating_add(1);
        }

        // The ceiling, which the clock rule above cannot enforce on its own
        // because a mark protects a thread indefinitely. Stalest first, and a
        // thread *currently blocked on a human* is never a candidate however old
        // it is — a fleet of blocked threads pushes the ceiling rather than
        // being silently dropped by it, because the whole product is getting to
        // them.
        while self.threads.len() > MAX_THREADS {
            let victim = self
                .threads
                .values()
                .filter(|t| t.status != ThreadStatus::Waiting)
                .min_by(|a, b| a.last_activity.cmp(&b.last_activity).then(a.id.cmp(&b.id)))
                .map(|t| t.id.clone());
            let Some(victim) = victim else {
                // Every thread is waiting on a human. The ceiling yields; the
                // alternative is deleting the marks the operator is looking for.
                break;
            };
            self.forget_thread(&victim);
            self.health.threads_retired = self.health.threads_retired.saturating_add(1);
            self.health.threads_evicted = self.health.threads_evicted.saturating_add(1);
        }
    }

    /// Closes one thread because the operator said so — the rail's `✕`.
    ///
    /// Every automatic rule here is a rule about *silence*, and silence is the
    /// only evidence a transcript can offer: there is no session-end record, so
    /// a session that ended, one that crashed and one still sitting at a prompt
    /// are indistinguishable (see `World::retire_threads`). The operator is
    /// the one party who actually knows, and until this existed there was no way
    /// to tell Polis — a thread pinned by a standing mark outlived every clock
    /// the world has.
    ///
    /// **Not a tombstone.** Nothing here remembers the dismissal, so the next
    /// event for that session builds the thread again through
    /// `World::thread_entry` exactly as a first sighting would. Closing an
    /// agent that turns out to still be working costs one row for one event,
    /// which is the right price: the alternative is a filter that hides a live
    /// agent, and a map that omits work is worse than one that shows work you
    /// had finished with.
    pub fn dismiss_thread(&mut self, id: &ThreadId) {
        if !self.threads.contains_key(id) {
            return;
        }
        self.forget_thread(id);
        self.health.threads_retired = self.health.threads_retired.saturating_add(1);
        self.health.threads_dismissed = self.health.threads_dismissed.saturating_add(1);
    }

    /// Closes one thread because its conversation was cleared.
    ///
    /// The one ending on this list that is **evidence rather than a timer**. A
    /// session that goes quiet might be finished or might be waiting for its
    /// operator, and `World::retire_threads` spends half an hour refusing to
    /// guess. `/clear` is not ambiguous: Claude Code abandons the transcript and
    /// opens a new one under the same `bridge-session.bridgeSessionId`, so the
    /// old file will never be appended to again and the operator cannot get back
    /// to that conversation either (`polis_ingest::live`, ADR-0105).
    ///
    /// # Why it is removed rather than marked done
    ///
    /// [`EventKind::SessionEnd`] finishes a thread and leaves it on the rail
    /// carrying its attention mark, because "this agent stopped, and its work is
    /// unverified" is a queue item. A cleared conversation is not: there is no
    /// thread left to go back to, and a *done, unverified* mark pinned to it
    /// would sit in the rail for [`MARK_HOLD_MAX`] naming a session the operator
    /// deliberately threw away. The buildings keep their height either way —
    /// uncommitted lines are a fact about the disk, not about the chat.
    ///
    /// Like [`World::dismiss_thread`], **not a tombstone**: a session that
    /// somehow speaks again rebuilds its thread on the next event.
    pub fn supersede_thread(&mut self, id: &ThreadId) {
        if !self.threads.contains_key(id) {
            return;
        }
        self.forget_thread(id);
        self.health.threads_retired = self.health.threads_retired.saturating_add(1);
        self.health.threads_cleared = self.health.threads_cleared.saturating_add(1);
    }

    /// Removes a thread and every reference to it, so nothing dangles.
    ///
    /// A half-removed thread is worse than a leaked one: an attention mark whose
    /// thread is gone has no position, a claim whose claimant is gone raises
    /// contention against nobody, and a `touched_by` entry pointing at a missing
    /// thread makes a building's drill-down list name a thread the rail cannot
    /// show. All four are cleared here, and the file walk is bounded by what the
    /// thread itself touched rather than by the size of the city.
    ///
    /// # In-flight calls go too, and that is not tidiness
    ///
    /// [`World::dismiss_thread`] is deliberately **not** a tombstone, and
    /// `ThreadId::of_session` is the identity function on the session id — so
    /// the next event for a dismissed session rebuilds the thread under the
    /// *same* [`ThreadId`]. An orphaned [`PendingCall`] left behind here would
    /// then be read by [`World::tick`] as evidence that the recreated thread has
    /// work in flight, and would hold it [`ThreadStatus::Working`] on a call
    /// belonging to a thread the operator deleted (see [`IN_FLIGHT_MAX`]).
    ///
    /// The price is that a `tool_result` arriving after the removal loses its
    /// diff and verification accounting. It already lost it: `apply::settle`
    /// looks the thread up in [`World::threads`] before recording anything, and
    /// that lookup misses on a thread that is gone.
    pub fn forget_thread(&mut self, id: &ThreadId) {
        let Some(thread) = self.threads.remove(id) else {
            return;
        };
        self.claims.release_thread(id);
        self.attention.retain(|m| !m.kind.mentions_thread(id));
        for path in thread.visits.keys() {
            if let Some(file) = self.files.get_mut(path) {
                file.touched_by.retain(|t| t != id);
            }
        }
        self.files_generation = self.files_generation.wrapping_add(1);
        self.spawns.retain(|_, t| t != id);
        self.workflow_runs.retain(|_, t| t != id);
        self.pending.retain(|_, c| &c.thread != id);
    }

    /// Drops unattributable workers that are never going to be adopted.
    ///
    /// [`UnattributedWorker`] is the honest model of a worker with no parent
    /// link, and honesty is not a licence to remember it for ever: the map is
    /// about what is happening now, and a worker last seen an hour ago is not.
    fn expire_unattributed(&mut self, now: Instant) {
        let before = self.unattributed.len();
        self.unattributed
            .retain(|_, w| now.saturating_duration_since(w.last_seen) <= UNATTRIBUTED_TTL);
        let dropped = before - self.unattributed.len();
        if dropped > 0 {
            self.health.unattributed_workers =
                u64::try_from(self.unattributed.len()).unwrap_or(u64::MAX);
        }
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
    ///
    /// **The hue ring is a live layer and is cleared with the rest of them.**
    /// Be precise about what that buys, because it is not what it first looks
    /// like: `HueRing` remembers *which id* holds each slot, so re-applying
    /// the **same** schedule over a stale ring would find every id already
    /// placed and hand out the same colours anyway — the backwards seek at
    /// `replay::ReplayDriver::seek`, the only caller in the product today, is
    /// idempotent either way. What this line prevents is a world reset onto a
    /// **different** set of events: without it the twelve slots stay spoken for
    /// by twelve ids that no longer exist, and the new city is painted entirely
    /// in bare preferences with [`Health::identity_hues_exhausted`] counting
    /// every thread in it. That is what "clears every live layer" has to mean
    /// for this one to be true, and it is asserted by
    /// `a_reset_world_hands_out_hues_as_a_fresh_one_does`.
    pub fn reset(&mut self, now: Instant) {
        self.now = now;
        self.hues = HueRing::default();
        self.layers = LayerRing::default();
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

    /// Whether the quiet-thread clock runs.
    ///
    /// On for a live world, off for one built by [`World::for_replay`]. The
    /// asymmetry is about **destructiveness**, not about replay being special:
    /// every other time-based rule here — the `Working` decay, territory
    /// dormancy, the trail fade — recomputes from `last_activity` and so lands
    /// on the same answer however the clock got there. Retirement deletes a
    /// thread outright, and a deleted thread rebuilt by its next event comes
    /// back with its counters at zero. That makes the world a function of the
    /// tick schedule, so a scrubber that seeks to a moment and one that plays
    /// through to it disagree — which is a replay that lies about what happened.
    pub fn set_retire_quiet_threads(&mut self, retire: bool) {
        self.retire_quiet_threads = retire;
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
    ///
    /// This is also the **only** place [`Thread::tint`] is decided (PRD §11.4).
    /// Creation is exactly the moment at which "which colours are already
    /// spoken for" is knowable, and doing it here rather than in [`Thread::new`]
    /// is what makes a hue exclusive rather than merely stable: see
    /// [`HueRing`].
    ///
    /// Written as a `contains_key` / `insert` pair rather than
    /// `entry().or_insert_with()` because the closure would have to borrow
    /// `self.hues` while `self.threads` is already borrowed by the entry. The
    /// extra lookup is on the cold path — once per session, not once per event.
    pub(crate) fn thread_entry(&mut self, session: &SessionId, at: Instant) -> &mut Thread {
        let id = ThreadId::of_session(session.clone());
        if !self.threads.contains_key(&id) {
            let mut thread = Thread::new(id.clone(), session.clone(), at);
            thread.layer = self.layers.assign(&id);
            match self.hues.assign(&id) {
                Some(slot) => thread.tint = slot,
                // Every hue this world has ever handed out is gone. `Thread::new`
                // has already set the bare preference, which is what the old
                // `thread_slot` would have returned — so the map degrades to the
                // behaviour that was there before rather than to no colour, and
                // the counter is what says so out loud.
                None => {
                    self.health.identity_hues_exhausted =
                        self.health.identity_hues_exhausted.saturating_add(1);
                }
            }
            self.threads.insert(id.clone(), thread);
        }
        self.threads.get_mut(&id).expect("just inserted")
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
        // Counted here rather than inside `Territory::observe`, which has no
        // `Health` to reach and is called on hand-built fixtures that are not
        // part of any run's census. The predicate is the territory's own — it is
        // `claim_path().is_root()` on both sides — and the pair of them is the
        // whole of what PRD §6.1's `Bash` cwd row actually contributes.
        if obs.claim_path().is_root() {
            self.health.root_scoped_observations += 1;
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

    /// Whether a tool call this thread issued is still outstanding.
    ///
    /// **Deliberately not bounded by [`IN_FLIGHT_MAX`].** `apply::derive` must be
    /// a pure function of the ledgers, or the same events replayed differently
    /// produce different states — `real_corpus`'s seek-determinism test caught
    /// exactly that when this read the clock. The staleness ceiling belongs to
    /// [`World::tick`], which already applies it, and which is where every other
    /// time-dependent demotion lives.
    pub(crate) fn pending_for(&self, thread: &ThreadId) -> bool {
        self.pending.values().any(|c| &c.thread == thread)
    }

    /// Rebuilds the contention marks from the live claim table.
    ///
    /// Contention is a **relation**, not a property, so it cannot outlive the
    /// pair of claims that produced it: rebuilding is cheaper and more honest
    /// than trying to expire the marks separately.
    fn refresh_contention(&mut self, now: Instant) {
        let mut hits = self.claims.hits(now);
        self.attention
            .retain(|m| !matches!(m.kind, attention::AttentionKind::Contention(_)));
        // Two workers of ONE agent on one file is not a collision (ADR: see
        // `CONTENTION_WITHIN_THREAD`). The orchestrator sequenced them; nobody's
        // work is being destroyed, and nothing is being asked of the operator.
        // Measured on the operator's own live map: 32 of 36 attention items were
        // this, every one of them ranked above the amber pins by §11.1's
        // ordering, and the operator's verdict on the result was "i dont need
        // the contention at all".
        //
        // §11.3's contention is "a relation between two threads", and this never
        // was one. It is dropped before the cap so it cannot spend the budget
        // that exists to keep real collisions visible.
        if !CONTENTION_WITHIN_THREAD {
            hits.retain(|h| !h.is_within_thread());
        }
        // `hits` is already worst-first. Past the ceiling the rest are counted
        // rather than drawn: the cap is on **contention only**, because it is
        // the only one of PRD §11.2's three states whose count grows with the
        // number of *files* rather than with the number of threads, and a cap on
        // the whole list would let a storm of red links bury every amber pin —
        // which is state (a), "the primary state; it is what the product is
        // for".
        let over = hits.len().saturating_sub(MAX_CONTENTION_MARKS);
        if over > 0 {
            hits.truncate(MAX_CONTENTION_MARKS);
            self.health.contention_over_cap = u64::try_from(over).unwrap_or(u64::MAX);
        } else {
            self.health.contention_over_cap = 0;
        }
        for hit in hits {
            let since = hit.challenger.at;
            self.attention.push(attention::Attention {
                kind: attention::AttentionKind::Contention(Box::new(hit)),
                since,
            });
        }
    }

    /// Rebuilds PRD §11.3's early warning: which threads' clouds overlap.
    ///
    /// > Two clouds overlapping means two orchestrators are claiming the same
    /// > district, and it fires *before* anyone collides — while redirecting one
    /// > is still cheap. Surface it as a **distinct, quieter signal** than
    /// > file-level contention.
    ///
    /// Quieter is implemented, not merely described: this list is separate from
    /// [`World::attention`], so an overlap can never take a slot from a red link
    /// or an amber pin, and it carries a score rather than a
    /// [`contention::Severity`] because nothing here is being destroyed yet.
    ///
    /// `since` is carried forward for a pair that was already overlapping, so a
    /// surface can fade it in over its own age. A signal that restarted its
    /// animation on every tick would be the opposite of quiet.
    fn refresh_overlaps(&mut self, now: Instant) {
        use std::collections::BTreeSet;

        let previous: BTreeMap<(ThreadId, ThreadId), Instant> = self
            .overlaps
            .iter()
            .map(|o| ((o.a.clone(), o.b.clone()), o.since))
            .collect();

        // Only *placed* territories have a cloud to overlap (PRD §6.2, §6.4), so
        // the pairing runs over those and not over every thread in the world —
        // and each is summarised **once**, not once per pairing. At PRD §16's
        // hundred threads that is the difference between 100 reductions and
        // 9 900 of them, which was 18.7 ms of a 16.6 ms frame
        // ([`contention::OVERLAP_KERNELS`]).
        //
        // `district` is the claim when there is one and the heaviest lobe
        // otherwise ([`territory::Placement::district`]). It is what
        // `TerritoryOverlap::same_district` compares, and an orchestrator's
        // heaviest lobe is the honest answer to "which district is this thread
        // in" — the alternative was to leave it out of the pairing entirely,
        // which is what a claim-only filter did and why §11.3's early warning
        // never fired for the threads it was written about.
        let placed: Vec<(&ThreadId, contention::CloudSummary, &LogicalPath)> = self
            .threads
            .values()
            .filter_map(|t| {
                let district = t.territory.placement().district()?;
                let summary = contention::CloudSummary::of(&t.territory)?;
                Some((&t.id, summary, district))
            })
            .collect();

        let mut out: Vec<contention::TerritoryOverlap> = Vec::new();
        for (i, (id_a, ta, claim_a)) in placed.iter().enumerate() {
            for (id_b, tb, claim_b) in placed.iter().skip(i + 1) {
                let Some(score) = contention::overlap_of(ta, tb) else {
                    continue;
                };
                // `placed` walks a `BTreeMap`, so `id_a < id_b` already; the
                // pair key is stable without sorting it again.
                let key = ((*id_a).clone(), (*id_b).clone());
                let since = previous.get(&key).copied().unwrap_or(now);
                out.push(contention::TerritoryOverlap {
                    a: key.0,
                    b: key.1,
                    claim_a: (*claim_a).clone(),
                    claim_b: (*claim_b).clone(),
                    score,
                    since,
                });
            }
        }
        // Worst first, then the same-district pairs, then by thread so the
        // order does not depend on how the pairing happened to run (PRD §7.4).
        out.sort_by(|x, y| {
            y.score
                .partial_cmp(&x.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(y.same_district().cmp(&x.same_district()))
                .then(x.a.cmp(&y.a))
                .then(x.b.cmp(&y.b))
        });
        let over = out.len().saturating_sub(contention::MAX_OVERLAPS);
        out.truncate(contention::MAX_OVERLAPS);
        self.health.overlaps_over_cap = u64::try_from(over).unwrap_or(u64::MAX);
        // Debug-only: the pair keys must be unique, or `since` would be carried
        // forward from whichever duplicate happened to be written last.
        debug_assert_eq!(
            out.iter()
                .map(|o| (&o.a, &o.b))
                .collect::<BTreeSet<_>>()
                .len(),
            out.len(),
            "one overlap per pair of threads"
        );
        self.overlaps = out;
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
    /// Working / Waiting / Interrupted / Parked / Ready / Idle / Done.
    ///
    /// Derived, never authored: every channel writes to the ledgers below and
    /// then calls `apply::derive`, which is the only function that assigns this.
    pub status: ThreadStatus,
    /// Work this thread launched that outlives the turn that launched it.
    ///
    /// A `run_in_background` shell hands its `tool_result` back in milliseconds
    /// and then runs for minutes, so this ledger is the only thing that can tell
    /// *finished* from *still going*. Non-empty at a turn boundary is what makes
    /// a thread [`ThreadStatus::Parked`] instead of [`ThreadStatus::Ready`].
    pub background: Vec<BackgroundTask>,
    /// Whether the last thing this thread said was the end of a turn.
    ///
    /// The ledger behind `Ready` / `Parked`. Set by a `stop_reason` boundary or
    /// a `Stop` hook, cleared by any activity, and read by `apply::derive` —
    /// which is what stops a finished turn from being confused with a thread
    /// that is mid-call and merely slow.
    pub turn_ended: bool,
    /// When the operator last pressed Esc, cleared by the thread's next activity.
    ///
    /// Stamped rather than written straight to `status` so that `derive` keeps
    /// its single-writer contract, and so an interrupt cannot outlive the work
    /// that follows it: measured on this machine, a `<task-notification>` can
    /// arrive 429 ms after an interrupt and the thread carries on.
    pub interrupted_at: Option<Instant>,
    /// This thread's identity hue slot, into `polis_render::live::THREAD_HUES`
    /// (PRD §11.4).
    ///
    /// Assigned **once**, by `World::thread_entry`, the first time the world
    /// sees the session, and never reassigned for the rest of the thread's
    /// life. Read as a field by every surface that draws the thread — the map,
    /// the rail, the cloud table, the headless frame — so there is exactly one
    /// place the answer is computed and the window and a recorded GIF cannot
    /// disagree about it.
    ///
    /// [`Thread::new`] defaults it to the thread's bare
    /// [`ThreadId::hue_preference`], which is what a `Thread` built outside a
    /// [`World`] gets: a fixture is not exclusive, deliberately, because
    /// exclusivity is a property of the *set* of live threads and a fixture has
    /// no set. Inside a world the ring overrides it.
    ///
    /// Never `polis_render::live::NO_TINT` (255): that value means *"nobody
    /// named an owner for this pixel"* and no thread may ever carry it.
    pub tint: u8,

    /// Which cloud layer this thread's field is drawn on — unique among the
    /// threads of one [`World`], and fixed for the thread's life.
    ///
    /// [`Thread::tint`] says what colour the cloud is; this says **which cloud
    /// it is**. The renderer groups kernels by it, the tween matches its two
    /// sides on it, and the tint table is indexed by it, so it has to be stable
    /// across frames and unique across threads — see [`LayerRing`] for why
    /// neither the tint nor the thread's rank in the visible list is either.
    ///
    /// [`Thread::new`] defaults it to the bare [`ThreadId::hue_preference`], on
    /// the same reasoning as `tint`: a fixture built outside a `World` is not
    /// exclusive, deliberately, because exclusivity is a property of the set.
    /// Inside a world the ring overrides it.
    pub layer: u16,

    /// The same steps as [`Thread::trail`] with PRD §10.1's shape channel
    /// attached: which operation, how it went, and which worker did it.
    ///
    /// `trail` is the geometry the renderer follows; `ops` is what it draws at
    /// each stop. Kept separate because §10.1 is emphatic that shape and colour
    /// are orthogonal channels and the trail's own encoding is neither.
    pub ops: VecDeque<Operation>,
    /// The agent's own one-line summaries of its recent calls, newest last.
    ///
    /// The only prose in the world that is not a name, and the narrowest
    /// channel that could carry intent — see [`Intent`] for why it is a
    /// separate ring rather than a field on [`Operation`], and what may never
    /// arrive on it.
    pub intents: VecDeque<Intent>,
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
        // Before `id` is moved into the struct, and defaulted to the bare
        // preference rather than left at zero: a fixture built outside a
        // `World` still gets the colour that thread has always had, and a
        // defaulted `0` would have made every fixture thread the same rose.
        let tint = id.hue_preference();
        let layer = u16::from(tint);
        Self {
            id,
            session_id,
            worktree: None,
            territory: territory::Territory::default(),
            workers: Vec::new(),
            trail: VecDeque::new(),
            status: ThreadStatus::Working,
            background: Vec::new(),
            turn_ended: false,
            interrupted_at: None,
            tint,
            layer,
            ops: VecDeque::new(),
            intents: VecDeque::new(),
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

    /// Appends one of the agent's own summaries, bounded and de-duplicated.
    ///
    /// A repeat of the newest line is dropped rather than stacked: an agent
    /// retrying one command writes the same sentence three times, and three
    /// copies of *"Run the workspace tests"* would crowd out the three
    /// different things it did before them.
    pub(crate) fn push_intent(&mut self, intent: Intent) {
        if intent.text.is_empty() {
            return;
        }
        if self.intents.back().is_some_and(|b| b.text == intent.text) {
            return;
        }
        self.intents.push_back(intent);
        while self.intents.len() > INTENT_CAP {
            self.intents.pop_front();
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

/// One job a thread launched that outlives the turn that launched it.
///
/// # Why this has to exist at all
///
/// `Bash(run_in_background: true)` returns its `tool_result` immediately — a
/// seven-minute `cargo test` is a 0.2-second tool call as far as every clock in
/// this crate is concerned. The thread then ends its turn and sits silent until
/// a `<task-notification>` wakes it. Without this ledger those minutes are
/// indistinguishable from a finished session, which is exactly what the operator
/// reported: *"it's waiting on the clean run before committing, not for the
/// user"*.
///
/// Measured on this machine: 360 distinct `backgroundTaskId`s across 125
/// transcripts, and `grep -rn backgroundTaskId --include=*.rs` matched **no
/// production file** before this change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTask {
    /// Claude Code's own `backgroundTaskId`, and the join key the closing
    /// `<task-notification>` names.
    pub id: String,
    /// The `tool_use` that launched it, so the job can be traced to its call.
    pub tool_use: Option<String>,
    /// What it is, in the operator's words — the command line where the launch
    /// record carried one.
    pub label: Option<String>,
    /// When it was launched.
    pub since: Instant,
}

impl BackgroundTask {
    /// How long it has been running.
    pub fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.since)
    }
}

/// What a thread is doing (PRD §5).
///
/// # One state per piece of evidence
///
/// Each variant is reached by a distinct thing a channel can actually *prove*,
/// which is the standard the rest of this crate holds itself to. The state that
/// used to be missing was [`ThreadStatus::Ready`], and its absence sent every
/// completed turn into [`ThreadStatus::Waiting`] — the loudest state in the
/// product, fired by the most common thing an agent does.
///
/// | Evidence | State |
/// |---|---|
/// | a live `NeedsDecision` mark | `Waiting` |
/// | a `[Request interrupted by user]` record | `Interrupted` |
/// | tool calls arriving | `Working` |
/// | turn ended with a background task still open | `Parked` |
/// | `stop_reason: end_turn`, no tool call, main agent | `Ready` |
/// | [`IDLE_AFTER`] of silence mid-turn, or a `StopFailure` | `Idle` |
/// | a `Stop` hook | `Done` |
///
/// # Only two of these are the operator's move
///
/// [`ThreadStatus::blocks_operator`] is the bit that matters, and it is true for
/// exactly `Waiting` and `Interrupted`. Everything else is the thread's own
/// business, however it got there. The failure this enum was widened for was one
/// word — `WAITING` — standing for "asked you a question", "finished", "you hit
/// Esc" and "a background `cargo test` is still going", which are four different
/// answers to *should I go there now*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadStatus {
    /// Actively calling tools.
    Working,
    /// Blocked on a human. This is what the product is for.
    Waiting,
    /// The operator pressed Esc. Nothing moves until they type.
    ///
    /// Distinct from [`ThreadStatus::Waiting`] because nothing was *asked* —
    /// there is no question to answer, no pin to resolve, and the thread stopped
    /// mid-thought rather than at a boundary. Distinct from
    /// [`ThreadStatus::Idle`] because the transcript carries positive proof, and
    /// reading an interrupt as silence is what produced the operator's report:
    /// *"i just interrupted this chat, the chat says 'idle', it should say
    /// 'interrupted'"*.
    ///
    /// Both wordings count. Measured over this machine's corpus: 45 records read
    /// `[Request interrupted by user]` and 41 read `[Request interrupted by user
    /// for tool use]` — 48% of all interrupts — and 15 of that second group are
    /// the last record in their file. Treating the second as a mere tool
    /// rejection leaves half of all interrupts decaying into `Idle`.
    Interrupted,
    /// Its turn ended, but work it started is still running and it will resume
    /// on its own. **Not the operator's move.**
    ///
    /// This is the state behind *"its waiting on the clean run before
    /// committing, not for the user"*. A `run_in_background` shell returns its
    /// `tool_result` in milliseconds while the command runs for minutes, so the
    /// thread looks finished to every clock Polis owns; then a
    /// `<task-notification>` arrives and it carries on by itself. Rendering that
    /// as `WAITING` told the operator to go somewhere nothing was needed of
    /// them, and rendering it as `Ready` would claim the prompt is theirs when
    /// the thread is about to take it back.
    Parked,
    /// Its turn ended and the prompt is the operator's.
    ///
    /// Not [`ThreadStatus::Waiting`]: nothing was asked, so nothing is blocked
    /// and no wall clock is being burned. Not [`ThreadStatus::Idle`]: the
    /// transcript carries positive proof the turn ended (`stop_reason`), rather
    /// than mere silence. Not [`ThreadStatus::Done`]: the session is alive and
    /// the operator's next message resumes it.
    ///
    /// **It does not decay.** "This thread's last turn ended and nothing has
    /// happened since" stays true indefinitely, and ageing it into `Idle` would
    /// re-create the complaint `Idle` already carries — *"the ones saying idle
    /// are completely unclear what they are doing"*. Retirement clears it on the
    /// ordinary timer, which it can now reach, because a ready thread carries no
    /// attention mark pinning it (see [`MARK_HOLD_MAX`]).
    Ready,
    /// Alive but quiet, with nothing to say why: it was working and went silent
    /// mid-turn, or its turn ended on an API error.
    Idle,
    /// Finished. Split by verification in [`attention`], because "done,
    /// unverified" is really "needs review".
    Done,
}

impl ThreadStatus {
    /// Status-rail order: waiting first, because unblocking is the primary
    /// decision PRD §1 exists to accelerate.
    ///
    /// [`ThreadStatus::Ready`] outranks [`ThreadStatus::Working`] on the same
    /// principle: the ordering is *what needs the operator, first*, and a thread
    /// whose turn has ended needs them while a working one does not.
    pub fn rail_rank(self) -> u8 {
        match self {
            Self::Waiting => 0,
            Self::Interrupted => 1,
            Self::Ready => 2,
            Self::Parked => 3,
            Self::Working => 4,
            Self::Idle => 5,
            Self::Done => 6,
        }
    }

    /// Whether the operator has to act before this thread can move.
    ///
    /// The one bit the rail sorts and colours by. `Parked` and `Ready` are
    /// deliberately false: a parked thread resumes itself, and a ready one is an
    /// invitation, not a demand.
    pub fn blocks_operator(self) -> bool {
        matches!(self, Self::Waiting | Self::Interrupted)
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
            Self::Interrupted => "interrupted",
            Self::Parked => "parked",
            Self::Ready => "ready",
            Self::Idle => "idle",
            Self::Done => "done",
        })
    }
}

/// One line the agent wrote about its own call, kept so something can say what
/// a thread is *doing* rather than only where it is.
///
/// # Why this is the only prose in the world
///
/// Every other field here is a name, a count, a time or an enum. The map can
/// therefore say where a thread is working, how hard, and whether it is stuck,
/// and it cannot say what the work *is* — the operator's report was that the
/// notation is exact and unreadable at the same time. Intent is not recoverable
/// from tool kinds and paths: `Edit src/auth/token.rs` is equally *"adding the
/// refresh path"* and *"reverting yesterday's refresh path"*.
///
/// It is recoverable from the transcript, because Claude Code asks for it. The
/// `Bash`, `PowerShell` and `Agent` tools all carry a `description` in their
/// input — *"Run the workspace tests"*, *"Find callout usage in mapview"* — and
/// it is the agent's own one-line summary of the call it is about to make.
/// Twenty of those is a near-complete account of what a thread has been up to,
/// written by the only party that knows.
///
/// # What may never arrive here, and why it is a separate ring
///
/// A `description` is prose *about* the work. A tool's other inputs are the
/// work: `Write.content` is a whole file, `Edit.old_string` is a diff, and
/// `Bash.command` is a command line that routinely holds a token. ADR-0005 is
/// what strips those out of the hook channel, and this ring is deliberately not
/// a loosening of it — it is a second, narrower door that only `description`
/// fits through, and `apply::assistant` is the one place that reads it.
///
/// It is a ring on the thread rather than a field on [`Operation`] for the same
/// reason: an `Operation` is cloned into the census and walked by the renderer
/// every frame, and a `String` on it would be both a hot-path cost and a field
/// that every future caller could quietly fill with something else. Nothing
/// draws an `Intent`. Only [`crate::Thread::intents`] holds one, and the only
/// consumer is a caption.
#[derive(Debug, Clone)]
pub struct Intent {
    /// Which tool the agent was describing. `Agent` means it was handing this
    /// sentence to a subagent, which is a different claim from doing it itself.
    pub tool: ToolKind,
    /// The agent's own words, stripped of control characters and bounded to
    /// [`INTENT_TEXT_MAX`].
    pub text: String,
    /// When the call it describes was made.
    pub at: Instant,
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
    /// Observations whose claimed path is the **repository root**, so they fed
    /// nothing at all — no evidence, no kernel, no ancestor.
    ///
    /// `Territory::observe` drops them for a good reason (the root is the
    /// absorbing element of PRD §6.2's ancestor, and its district centre is the
    /// middle of the map), and the drop is invisible: the operation still draws
    /// its own mark and still counts in [`territory::Territory::observations`],
    /// so nothing on any surface said that more than half the evidence stream
    /// was being discarded. On the corpus this counter was added against it read
    /// **11 605 of 20 246 — 57.3 %**, because a shell call's only path signal is
    /// its `cwd` and a `cwd` at the checkout root resolves to the root.
    ///
    /// It is the number that makes "why has this thread no cloud" answerable
    /// from the status bar instead of from a debugger, which is the same
    /// argument [`Health::unplaced_observations`] beside it was added on.
    pub root_scoped_observations: u64,
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
    ///
    /// **Counts hits, not edits.** It is incremented only where a claim
    /// actually collided with another and the pair had no line range to tier
    /// on, so it answers *"how much of the red on this map is imprecise"*. The
    /// far larger number of edits that merely *lack* a line range is
    /// [`Health::edits_without_line_ranges`]; conflating the two made this read
    /// 1 768 on a session with 8 contention hits, which is not an answer to any
    /// question an operator has.
    pub contention_without_line_ranges: u64,
    /// Mutating tool results whose diff had to be approximated because
    /// `structuredPatch` was absent (ADR-0004).
    ///
    /// The denominator behind [`FileState::diff_precision`] being
    /// [`DiffPrecision::Approximate`], and the measure of how much of Channel
    /// D's 65 % subagent gap this run actually hit. It is **not** a contention
    /// number: none of these edits need have collided with anything.
    pub edits_without_line_ranges: u64,
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
    /// Live contention hits that did not fit inside [`MAX_CONTENTION_MARKS`].
    ///
    /// Non-zero means the map is showing the worst thirty-two collisions and
    /// there are more. Counted rather than hidden, because "how bad is it" is
    /// the question a red link is answering.
    pub contention_over_cap: u64,
    /// Territory overlaps that did not fit inside
    /// [`contention::MAX_OVERLAPS`].
    ///
    /// The early warning's own overflow. Non-zero means more than sixteen pairs
    /// of orchestrators are working in each other's districts, which is itself
    /// the answer to "should I redirect somebody".
    pub overlaps_over_cap: u64,
    /// Logical paths the claim table dropped at
    /// [`contention::MAX_CLAIMED_PATHS`].
    ///
    /// Mirrors [`contention::ClaimTable::paths_evicted`] into the status bar:
    /// non-zero means some collisions were structurally unobservable, which the
    /// operator must be told rather than left to assume.
    pub claim_paths_evicted: u64,
    /// Threads retired for silence, or evicted by [`MAX_THREADS`].
    ///
    /// The number an operator checks when the rail is shorter than the number of
    /// agents they know they started: a session that has been quiet for
    /// [`THREAD_RETIRE_AFTER`] leaves the map, and this is the count of them.
    pub threads_retired: u64,
    /// Of those, the ones the operator closed by hand from the rail's `✕`
    /// ([`World::dismiss_thread`]).
    ///
    /// Separate from the clock's count because it answers a different question:
    /// a rail shorter than the fleet is expected after a dismissal and is a bug
    /// report without one.
    pub threads_dismissed: u64,
    /// Of those, the ones whose conversation was cleared
    /// ([`World::supersede_thread`]).
    ///
    /// Also separate, and for the same reason: these left the map on *proof*
    /// rather than on the clock, so a rail that is short by this many is short
    /// because the operator ended those chats, seconds ago, not because
    /// [`THREAD_RETIRE_AFTER`] eventually gave up on them.
    pub threads_cleared: u64,
    /// Of those, the ones the [`MAX_THREADS`] ceiling took rather than the
    /// clock.
    ///
    /// Non-zero means more threads were live at once than the ceiling allows,
    /// which is a fact about the fleet and not a fault — but it is the only way
    /// to tell "the rail is short because agents finished" from "the rail is
    /// short because it ran out of room".
    pub threads_evicted: u64,
    /// Threads created after all [`IDENTITY_SLOTS`] identity hues had been
    /// handed out, so they fell back to their bare
    /// [`ThreadId::hue_preference`] and are **certain** to share a colour with
    /// an earlier thread (PRD §11.4).
    ///
    /// The exclusivity this counter measures the exhaustion of is the whole
    /// point of the change that added it — the operator's report was *"the same
    /// color is super bad"* — so the failure mode is the reported bug coming
    /// back. It must be visible rather than inferred: non-zero means "two rows
    /// in the rail can be the same colour again, and here is how many".
    ///
    /// Slots are never released, only claimed (see `HueRing` for why the
    /// alternative breaks window/headless parity), so this counts threads past
    /// the world's **twelfth distinct id ever**, not its twelfth concurrent. A
    /// Polis left open across a day of short sessions reaches it with two
    /// threads on screen. A session that goes quiet, is retired and comes back
    /// does **not** count here: it keeps the slot it had.
    pub identity_hues_exhausted: u64,
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

    /// The ring's own arithmetic, which is private and so can only be asserted
    /// here. The behaviour it produces through a real `World` — exclusivity,
    /// the reset, and window/headless parity — is in
    /// `polis-world/tests/identity_hue.rs`.
    #[test]
    fn the_hue_ring_walks_up_from_the_preference_and_then_gives_up() {
        let id = |s: &str| ThreadId::of_session(SessionId::new(s));
        let mut ring = HueRing::default();

        // Two ids that want slot 4 (pinned in `polis_events::ids`). The first
        // gets it; the second walks one slot up, never down and never to an
        // arbitrary free slot, so displacement is as local as it can be.
        assert_eq!(ring.assign(&id("a")), Some(4));
        assert_eq!(ring.assign(&id("polis")), Some(5));
        // Asking again is a lookup, not a claim: it consumes nothing, which is
        // what makes a retired-and-rebuilt thread free.
        assert_eq!(ring.assign(&id("a")), Some(4));

        // Fill the rest, then confirm the ring says "gone" rather than
        // over-writing somebody.
        let mut taken: Vec<u8> = vec![4, 5];
        for n in 0..10 {
            let slot = ring
                .assign(&id(&format!("filler-{n}")))
                .expect("a free slot");
            assert!(!taken.contains(&slot), "slot {slot} was handed out twice");
            taken.push(slot);
        }
        assert_eq!(taken.len(), IDENTITY_SLOTS as usize);
        assert_eq!(ring.assign(&id("one-too-many")), None);
        // And the ids already in it are still answered after exhaustion.
        assert_eq!(ring.assign(&id("polis")), Some(5));
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

    /// `cargo test` from the checkout root, through the world rather than
    /// through [`territory::Territory`] alone.
    ///
    /// PRD §6.1's table gives `Bash` cwd a weight of 0.5, and for the common
    /// case — shell input carries `command` and never `path`, so the cwd
    /// fallback fires and resolves to the checkout root — that row buys exactly
    /// nothing about *where*. It has always bought something about *whether*,
    /// and until this change nothing collected it: the observation stepped the
    /// trail and stamped `Thread::last_activity`, and then the territory quietly
    /// forgot it had ever happened and told PRD §10.4's dormancy gate the thread
    /// had stopped.
    ///
    /// This is the join with the idle fix in one assertion block: one call, four
    /// consumers, and the only one that is allowed to ignore it is the ancestor.
    #[test]
    fn a_root_scoped_shell_call_still_proves_the_thread_is_alive() {
        let mut world = World::for_replay(CityLayout::default());
        let session = SessionId::new("s");
        let at = Instant::now();
        world.thread_entry(&session, at);
        let id = ThreadId::of_session(session);

        world.observe(&Observation {
            thread: id.clone(),
            worker: None,
            path: LogicalPath::root(),
            scope: PathScope::Directory,
            tool: ToolKind::Bash,
            at,
            weight: None,
        });

        assert_eq!(
            world.health.root_scoped_observations, 1,
            "counted, so the drop is visible in the status bar rather than \
             guessed at from an empty map"
        );
        let thread = world.threads.get(&id).expect("the thread");
        assert!(
            thread.territory.evidence.is_empty(),
            "the root is the absorbing element of PRD §6.2's ancestor and says \
             nothing about where"
        );
        assert_eq!(
            thread.territory.observations, 1,
            "but the thread did do that work"
        );
        assert_eq!(
            thread.last_activity, at,
            "and the rail's clock saw it (the idle fix's half)"
        );
        assert_eq!(thread.trail.len(), 1, "and PRD §12's trail stepped");
        // The dormancy clock is the half this change adds. `quiet_for` needs
        // `decay` to have run — `last_decay` **is** now — and `observe` runs it.
        assert_eq!(
            thread.territory.quiet_for(),
            Some(Duration::ZERO),
            "and PRD §10.4's dormancy clock saw it too"
        );
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
