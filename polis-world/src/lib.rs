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

pub mod attention;
pub mod contention;
pub mod snapshot;
pub mod territory;

use std::collections::BTreeMap;
use std::time::Instant;

use polis_events::{
    AgentType, Event, LogicalPath, SessionId, ThreadId, ToolKind, WorkerId, WorktreeId,
};
use polis_layout::CityLayout;
use polis_repo::RepoTree;
use smallvec::SmallVec;

/// Everything Polis knows (PRD §5).
///
/// `BTreeMap` throughout, not `HashMap`: PRD §7.4 forbids iteration order from
/// reaching anything the layout or a golden file can see.
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
}

impl World {
    /// Builds an empty world over a repository and its city.
    pub fn new(repo: RepoTree, layout: CityLayout) -> Self {
        let _ = (repo, layout);
        todo!("PRD §5 — the single-writer state")
    }

    /// Applies one event. The only mutation path.
    pub fn apply(&mut self, event: &Event) {
        let _ = event;
        todo!("PRD §5 — normalise into World; unknown events are dropped, never fatal")
    }

    /// Advances decay, TTLs and hysteresis to `now`.
    ///
    /// Called once per batch, not per event, so territory half-life and claim
    /// expiry do not depend on how chatty the fleet is.
    pub fn tick(&mut self, now: Instant) {
        let _ = now;
        todo!("PRD §6.3, §11.3 — 90 s territory half-life, 30 s claim TTL")
    }
}

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
    pub trail: std::collections::VecDeque<(LogicalPath, Instant)>,
    /// Working / Waiting / Idle / Done.
    pub status: ThreadStatus,
}

/// A subagent (PRD §3).
#[derive(Debug, Clone)]
pub struct Worker {
    /// `agent_id`. Presence of this, and nothing else, is what makes a record a
    /// worker's.
    pub id: WorkerId,
    /// `Explore`, `Plan`, `general-purpose`, `workflow-subagent`, a custom
    /// frontmatter name. A type, never an identity.
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

/// Live per-file state (PRD §5).
#[derive(Debug, Clone, Default)]
pub struct FileState {
    /// Uncommitted diff lines. Drives building height (PRD §7.3).
    pub diff_lines: u32,
    /// Last write.
    pub last_touched: Option<Instant>,
    /// Last time a test ran against this file after a change. `None` here is
    /// what makes a finished thread "done, unverified" rather than "done".
    pub last_verified: Option<Instant>,
    /// Which threads have touched it. Four inline covers essentially every real
    /// file; contention needs only two.
    pub touched_by: SmallVec<[ThreadId; 4]>,
}

/// Degraded-channel and drift reporting for the status bar (PRD §4.5, §17).
///
/// > a schema-drift warning in the status bar rather than a crash.
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
}

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
    /// When Polis observed it — the hook or OTel clock, never a transcript
    /// timestamp (ADR-0014).
    pub at: Instant,
}
