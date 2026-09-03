//! Contention detection (PRD §11.3).
//!
//! > On `PreToolUse` for `Edit`/`Write`, register a claim on the **logical path**
//! > keyed by thread, with a 30s TTL. A second live claim on the same logical
//! > path is a hit.
//!
//! # Claims are keyed by *actor*, not by thread
//!
//! The PRD says "keyed by thread", and a thread is a main agent **plus its
//! worker subtree** (PRD §3). Keyed literally, a claim table can therefore never
//! fire between two workers of one session — and that is now the common case,
//! not an edge case: the operator's own sessions reach 69 workers, and a session
//! that fans out to a dozen parallel subagents is a fleet inside one `ThreadId`.
//!
//! Measured on the operator's real corpus, thread-keying was hiding real
//! collisions. In one 69-worker session **77 logical paths were written by more
//! than one worker**, and elsewhere two subagents wrote `settings.css` 4.7 s
//! apart — inside the 30 s TTL, on one branch, in one checkout. That is
//! `Severity::High` at least, it is the state PRD §11.1 ranks above everything
//! else because *work is actively being destroyed*, and thread-keying rendered
//! exactly nothing.
//!
//! So a claim is keyed by [`Actor`] — `(thread, worker)` — and a hit is a second
//! live claim **from a different actor**. Two claims from one actor still
//! refresh rather than collide, which is what a single agent editing one file
//! repeatedly looks like. [`Contention::is_within_thread`] says which kind a hit
//! is, so a surface that wants to phrase them differently can, and
//! [`Contention::actors`] names both ends.
//!
//! # A landed write keeps contending; only an abandoned one is released
//!
//! Keying alone did not make the operator's collisions render, and the number
//! that says so is `0`. Replaying the two sessions named above with claims keyed
//! by actor produced **zero** contentions out of 39 366 events — because a claim
//! was *deleted* the moment its write landed, and in a transcript a `tool_result`
//! follows its `tool_use` by well under a second. Two workers 4.7 s apart never
//! held a claim at the same instant, so there was nothing left to collide.
//!
//! That also made [`Severity::Critical`] unreachable in practice. A line range
//! only exists in `structuredPatch`, which arrives *with the result* — so the
//! claim that carries one was registered and deleted inside a single function
//! call, microseconds apart, and could never be live when anything else arrived.
//!
//! The fix is to say what the 30 s TTL was always saying. A claim has a
//! [`ClaimPhase`]:
//!
//! * **[`ClaimPhase::Pending`]** — registered from `PreToolUse` or a `tool_use`
//!   block. The bytes are not on disk yet.
//! * **[`ClaimPhase::Landed`]** — the write was observed landing. The bytes
//!   *are* on disk, and the window in which a sibling can write over them has
//!   just **begun**, not ended.
//!
//! So [`ClaimTable::landed`] converts a claim and restarts its TTL from the
//! landing, while [`ClaimTable::release`] — the early release — deletes one, and
//! is called exactly when the write **did not happen**: denied, errored, or
//! abandoned. That is the case early release exists for, because a reservation
//! that will never be exercised should not hold a file for thirty seconds. A
//! write that *did* happen is not a reservation any more; it is a hazard, and
//! PRD §11.3's thirty seconds is its duration.
//!
//! The one tier that reads the phase is [`Severity::Low`], because *"one writing
//! while another reads"* is about a write **in flight**: reading a file someone
//! finished writing three seconds ago returns the new bytes, and calling that a
//! stale read would be the fabrication ADR-0004 forbids.
//!
//! ## Why this does not manufacture hits
//!
//! Two channels reporting *the same physical edit* must not collide with
//! themselves, and they cannot, because only two paths register a claim and both
//! carry the same worker for one edit: Channel B's `PreToolUse` (whose
//! `agent_id` is present on 100 % of hook payloads fired inside a subagent —
//! `docs/verified/hooks-schema.md` §2.1) and Channel D's `assistant` record
//! (whose worker comes from the record's own `agentId`, or failing that from the
//! `agent-<id>.jsonl` file it was read from). Channel A registers no claims at
//! all — which matters, because `agent_id` appears **only** on the
//! `claude_code.llm_request` span and never on a logs-channel tool record
//! (`docs/verified/otel-schema.md`), so an OTel-derived claim would carry
//! `worker: None` for a subagent's edit and collide with that subagent's own.
//! Channel A only ever *lands* or *releases* a claim, and one that matches no
//! actor simply leaves the TTL to do its job.
//!
//! # `PreToolUse` is mandatory, not a fallback
//!
//! PRD §11.3 tiers severity by line range, and the transcript is the only source
//! of line ranges — but `toolUseResult`, which carries `structuredPatch`, is
//! **absent on 65% of subagent tool results** (14 811 of 22 697), including 625
//! `Edit` calls, while being 100% present on main-thread results. So the
//! transcript cannot supply the line-range tier for *any* subagent edit.
//!
//! `PreToolUse` claims are therefore promoted from fallback to the primary
//! contention channel, and JSONL-derived contention degrades from "overlapping
//! line ranges" to "same file" whenever the record came from a subagent
//! (ADR-0004). The `FileChanged` hook is not an alternative: it carries no
//! `tool_name` and no `tool_use_id` (ADR-0003).
//!
//! When that degradation happens, [`Contention::precision`] says so, and
//! [`crate::Health::contention_without_line_ranges`] counts it. Contention is
//! the only attention state where work is actively being destroyed; a fabricated
//! Critical would make the one thing the operator must trust the one thing they
//! cannot.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use polis_events::{LogicalPath, ThreadId, WorkerId, WorktreeId};

/// How long a claim stays live (PRD §11.3).
pub const CLAIM_TTL: Duration = Duration::from_secs(30);

/// Most simultaneous claimants remembered for one logical path.
///
/// **Measured under PRD §16's synthetic load.** With claims keyed by actor, a
/// hundred threads of four hundred workers each put twenty-five live claimants
/// on some files, and the cross product of those is 300 relations *per file* —
/// 480 000 attention marks in all, a 1.45 s `tick` and a 0.68 s publish against
/// PRD §13.1's 16.6 ms frame. Contention became unrenderable at exactly the
/// scale that produces it.
///
/// Eight is a ceiling on the *relation*, not on the truth: every claim is still
/// registered and refreshed, the oldest is the one dropped, and
/// [`ClaimTable::claims_on`] is unaffected — so PRD §12's *"hovering a building
/// must give a definite list of which threads touched it and when"* still gets
/// its list. What is bounded is how many pairings one file can generate, and
/// eight agents on one file is already past the point where a ninth changes a
/// decision (PRD §17).
pub const MAX_CLAIMS_PER_PATH: usize = 8;

/// Most logical paths the table holds claims on at once.
///
/// A claim used to vanish within a second of being made, so the table was
/// self-limiting by accident. Now that a landed write holds its path for the
/// rest of the 30 s TTL, the number of *paths* is bounded only by how fast a
/// fleet can write — and [`ClaimTable::hits`] walks every one of them on every
/// tick, inside PRD §13.1's frame budget.
///
/// Four thousand is far above what the operator's own fan-out sessions reach
/// (the busiest wrote 95 multi-worker paths across its whole life) and far below
/// where the walk costs a frame. Past it the path whose newest claim is oldest
/// is dropped: it is the one closest to expiring anyway, and dropping the
/// freshest would hide the write that just happened.
pub const MAX_CLAIMED_PATHS: usize = 4096;

/// Whether the write a claim covers has happened yet.
///
/// See this module's header: the distinction is what makes the 30 s TTL mean a
/// *window* rather than the duration of a tool call, and it is the difference
/// between a reservation (which early release cancels) and a hazard (which it
/// must not).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClaimPhase {
    /// Registered from `PreToolUse` or a `tool_use` block. The bytes are not on
    /// disk yet, so another agent reading this file right now may be reading
    /// content that is about to change — PRD §11.3's [`Severity::Low`].
    Pending,
    /// The write was observed landing, by Channel A's tool span, Channel C's
    /// `Modified`, or Channel D's `tool_result`. The bytes are on disk and the
    /// window in which a sibling can write over them has begun.
    Landed,
}

/// Who holds a claim: a thread, or one worker inside it.
///
/// The key the claim table collides on. See this module's header for why it is
/// not the [`ThreadId`] alone — in short, because two workers of one session
/// clobbering one file is the common case on real sessions and thread-keying
/// makes it unrenderable.
///
/// `None` for the worker means the main agent itself, which is a distinct actor
/// from every worker it spawned: a main agent editing a file its own subagent is
/// editing is the same collision as any other.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Actor {
    /// The thread — a main agent plus its worker subtree (PRD §3).
    pub thread: ThreadId,
    /// The worker inside it, or `None` for the main agent.
    pub worker: Option<WorkerId>,
}

impl Actor {
    /// The main agent of a thread.
    pub fn main(thread: ThreadId) -> Self {
        Self {
            thread,
            worker: None,
        }
    }

    /// One worker of a thread.
    pub fn worker(thread: ThreadId, worker: WorkerId) -> Self {
        Self {
            thread,
            worker: Some(worker),
        }
    }

    /// Whether this actor is a subagent rather than the main agent.
    pub fn is_worker(&self) -> bool {
        self.worker.is_some()
    }
}

/// Live claims on logical paths.
///
/// Keyed on the **logical** path, so `/repo-wt-3/src/auth.ts` and
/// `/repo-wt-7/src/auth.ts` collide on one key. That collision is the entire
/// point of PRD §7.6: it is how the map shows two agents editing the same file
/// on different branches.
#[derive(Debug)]
pub struct ClaimTable {
    /// One entry per contended path; at most one claim per thread within it.
    live: BTreeMap<LogicalPath, Vec<Claim>>,
    ttl: Duration,
    /// Hits seen since start, for the status bar.
    total_hits: u64,
    /// Paths dropped by [`MAX_CLAIMED_PATHS`], for [`crate::Health`].
    paths_evicted: u64,
}

impl Default for ClaimTable {
    fn default() -> Self {
        Self {
            live: BTreeMap::new(),
            ttl: CLAIM_TTL,
            total_hits: 0,
            paths_evicted: 0,
        }
    }
}

impl ClaimTable {
    /// A table with a non-default TTL. Tests use this; production does not.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            ..Self::default()
        }
    }

    /// How long a claim stays live.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Registers a claim and returns any contention it creates.
    ///
    /// A second live claim on the same logical path from a **different**
    /// [`Actor`] is a hit; a second claim from the same actor refreshes the
    /// first, which is what one agent editing one file repeatedly looks like.
    ///
    /// When more than one other actor holds the path, the worst pairing is
    /// returned — the operator needs the severity, and the drill-down layer can
    /// ask [`ClaimTable::claims_on`] for the rest.
    pub fn claim(&mut self, claim: Claim) -> Option<Contention> {
        let ttl = self.ttl;
        let actor = claim.actor();
        let entry = self.live.entry(claim.path.clone()).or_default();
        entry.retain(|c| claim.at.saturating_duration_since(c.at) <= ttl);

        // Classified by reference and cloned once. Building a `Contention` per
        // incumbent copied two claims — two paths and two branch strings — for
        // every live claimant, which is what made the synthetic load above cost
        // 416 us an event.
        let worst = entry
            .iter()
            .filter(|c| c.actor() != actor)
            .max_by_key(|incumbent| classify(incumbent, &claim))
            .map(|incumbent| Contention::of(incumbent.clone(), claim.clone()));

        if let Some(slot) = entry.iter_mut().find(|c| c.actor() == actor) {
            *slot = claim;
        } else {
            entry.push(claim);
            // Past the ceiling, the *oldest* claim goes: it is the one closest to
            // expiring on its own, and dropping the newest would hide the arrival
            // that made this a hit.
            while entry.len() > MAX_CLAIMS_PER_PATH {
                let oldest = entry
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, c)| c.at)
                    .map(|(i, _)| i);
                match oldest {
                    Some(i) => {
                        entry.remove(i);
                    }
                    None => break,
                }
            }
        }
        if worst.is_some() {
            self.total_hits += 1;
        }
        self.enforce_path_cap();
        worst
    }

    /// Keeps the table inside [`MAX_CLAIMED_PATHS`].
    ///
    /// Evicts whole paths, stalest first, where "stalest" is the path whose
    /// *newest* claim is oldest — the one closest to expiring on the TTL anyway.
    fn enforce_path_cap(&mut self) {
        while self.live.len() > MAX_CLAIMED_PATHS {
            let stalest = self
                .live
                .iter()
                .filter_map(|(path, entry)| {
                    entry
                        .iter()
                        .map(|c| c.at)
                        .max()
                        .map(|newest| (newest, path))
                })
                .min_by(|(a, pa), (b, pb)| a.cmp(b).then(pa.cmp(pb)))
                .map(|(_, path)| path.clone());
            match stalest {
                Some(path) => {
                    self.live.remove(&path);
                    self.paths_evicted = self.paths_evicted.saturating_add(1);
                }
                None => break,
            }
        }
    }

    /// Records that the write a claim covered has **landed**.
    ///
    /// The claim is kept and its TTL restarts from `at`, because the window in
    /// which a sibling can write over those bytes begins when they reach the
    /// disk. This is what makes the operator's own collisions render: two
    /// workers 4.7 s apart never hold a *pending* claim at the same instant, and
    /// deleting the first one on landing is what made a fleet's worth of real
    /// clobbering invisible (see this module's header).
    ///
    /// Keyed by [`Actor`], like everything else here: a landing reported without
    /// an `agent_id` names the main agent and matches nothing when a subagent
    /// did the writing, which is correct — the TTL is the backstop.
    ///
    /// Does nothing when no claim matches, which is the normal case for
    /// Channel A.
    pub fn landed(&mut self, actor: &Actor, path: &LogicalPath, at: Instant) {
        let Some(entry) = self.live.get_mut(path) else {
            return;
        };
        if let Some(claim) = entry.iter_mut().find(|c| c.actor() == *actor) {
            claim.phase = ClaimPhase::Landed;
            // Never move a claim backwards: a late-arriving Channel A span can
            // report a landing Polis already heard about from Channel D.
            if at > claim.at {
                claim.at = at;
            }
        }
    }

    /// Probes the table with a read, without registering anything.
    ///
    /// This is PRD §11.3's Low tier — "one writing while another reads" — and it
    /// is a probe rather than a claim because registering every `Read` would
    /// make the table the ~200 calls/sec firehose PRD §4.2 exists to avoid.
    ///
    /// Only a **pending** write contends with a read. The tier is *"one writing
    /// while another reads"*, and a file whose write landed three seconds ago is
    /// simply a file: the reader gets the new bytes, and calling that a stale
    /// read would fabricate the one thing ADR-0004 says never to fabricate.
    pub fn note_read(&self, reader: &Claim) -> Option<Contention> {
        let ttl = self.ttl;
        let actor = reader.actor();
        self.live
            .get(&reader.path)?
            .iter()
            .filter(|c| {
                c.actor() != actor && c.kind == ClaimKind::Write && c.phase == ClaimPhase::Pending
            })
            .filter(|c| reader.at.saturating_duration_since(c.at) <= ttl)
            .max_by_key(|incumbent| classify(incumbent, reader))
            .map(|incumbent| Contention::of(incumbent.clone(), reader.clone()))
    }

    /// Releases a claim early, because the write it covered **did not happen**.
    ///
    /// This is PRD §11.3's early release, and this is the case it is for: a
    /// denied, errored or abandoned call registered a reservation that will
    /// never be exercised, and holding a file for thirty seconds on the strength
    /// of a write nobody made would raise contention against nothing.
    ///
    /// A write that *did* happen goes to [`ClaimTable::landed`] instead and
    /// keeps its claim — see this module's header for the zero that says why.
    ///
    /// Keyed by [`Actor`], like the claim itself: a release naming the main
    /// agent must not clear a *worker's* claim on the same file, because that is
    /// precisely the collision this table now exists to see. A release that
    /// matches no actor is normal — Channel A's tool result carries no
    /// `agent_id` at all — and the TTL is the backstop for it.
    pub fn release(&mut self, actor: &Actor, path: &LogicalPath) {
        if let Some(entry) = self.live.get_mut(path) {
            entry.retain(|c| c.actor() != *actor);
            if entry.is_empty() {
                self.live.remove(path);
            }
        }
    }

    /// Drops every claim held by a thread, whichever actor inside it holds it.
    ///
    /// Used when a thread is retired from the world ([`crate::World::tick`]): a
    /// claim that outlived its claimant would keep raising contention between a
    /// thread that exists and one that does not.
    pub fn release_thread(&mut self, thread: &ThreadId) {
        self.live.retain(|_, entry| {
            entry.retain(|c| &c.thread != thread);
            !entry.is_empty()
        });
    }

    /// Expires claims older than [`CLAIM_TTL`].
    pub fn expire(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.live.retain(|_, entry| {
            entry.retain(|c| now.saturating_duration_since(c.at) <= ttl);
            !entry.is_empty()
        });
    }

    /// Every live contention, worst first — **one per contended path**.
    ///
    /// Contention is a relation, so this is recomputed rather than remembered: a
    /// mark that outlived its pair of claims would be a red link between two
    /// threads that are no longer colliding.
    ///
    /// # Why one per path and not one per pair
    ///
    /// PRD §11.2 draws contention as *"a link joining them across the map"*, and
    /// a file with five claimants has ten pairs but one problem: **this file is
    /// being clobbered**. Ten links say it five times over, in the one colour the
    /// map reserves for the state where work is actively being destroyed, and
    /// PRD §17's test — *"does it change a decision?"* — answers for the second
    /// through tenth.
    ///
    /// The number that made it non-negotiable is in [`MAX_CLAIMS_PER_PATH`]:
    /// under PRD §16's synthetic load the cross product was 480 000 relations and
    /// a 1.45 s tick. The worst pairing is returned, so the severity an operator
    /// reads is still the worst one present, and [`ClaimTable::claims_on`] still
    /// has every claimant for the drill-down PRD §12 requires.
    pub fn hits(&self, now: Instant) -> Vec<Contention> {
        let ttl = self.ttl;
        let mut out = Vec::new();
        for entry in self.live.values() {
            let live: Vec<&Claim> = entry
                .iter()
                .filter(|c| now.saturating_duration_since(c.at) <= ttl)
                .collect();
            let mut worst: Option<(Severity, &Claim, &Claim)> = None;
            for (i, incumbent) in live.iter().enumerate() {
                for challenger in live.iter().skip(i + 1) {
                    if incumbent.actor() == challenger.actor() {
                        continue;
                    }
                    // The older claim is the incumbent, whichever order the
                    // table happens to hold them in.
                    let (a, b) = if incumbent.at <= challenger.at {
                        (*incumbent, *challenger)
                    } else {
                        (*challenger, *incumbent)
                    };
                    let severity = classify(a, b);
                    let better = match &worst {
                        None => true,
                        Some((s, ia, ib)) => {
                            // Ties break on the actors, so the pairing chosen for
                            // one path does not depend on iteration order
                            // (PRD §7.4).
                            (severity, a.actor(), b.actor()) > (*s, ia.actor(), ib.actor())
                        }
                    };
                    if better {
                        worst = Some((severity, a, b));
                    }
                }
            }
            if let Some((_, a, b)) = worst {
                out.push(Contention::of(a.clone(), b.clone()));
            }
        }
        out.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(a.incumbent.path.cmp(&b.incumbent.path))
                .then(a.incumbent.actor().cmp(&b.incumbent.actor()))
                .then(a.challenger.actor().cmp(&b.challenger.actor()))
        });
        out
    }

    /// Live claims on one path, for the drill-down layer.
    ///
    /// > Hovering a building must give a definite list of which threads touched
    /// > it and when. (PRD §12)
    pub fn claims_on(&self, path: &LogicalPath) -> &[Claim] {
        self.live.get(path).map_or(&[], Vec::as_slice)
    }

    /// Every claimed path with its live claims, in path order.
    ///
    /// The drill-down layer's wide view of PRD §12's *"a definite list of which
    /// threads touched it and when"*: [`ClaimTable::claims_on`] answers for one
    /// building, this answers for the map.
    pub fn paths(&self) -> impl Iterator<Item = (&LogicalPath, &[Claim])> + '_ {
        self.live.iter().map(|(p, e)| (p, e.as_slice()))
    }

    /// How many paths currently carry a claim.
    pub fn len(&self) -> usize {
        self.live.len()
    }

    /// True when nothing is claimed.
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// Hits seen since start.
    pub fn total_hits(&self) -> u64 {
        self.total_hits
    }

    /// Paths dropped because the table was at [`MAX_CLAIMED_PATHS`].
    ///
    /// Non-zero means a fleet is writing faster than the TTL retires files, and
    /// some collisions were structurally unobservable. Counted rather than
    /// hidden: contention is the one state the operator must be able to trust.
    pub fn paths_evicted(&self) -> u64 {
        self.paths_evicted
    }
}

/// Whether a claim intends to write or is only reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimKind {
    /// `Edit` / `Write` / `NotebookEdit` — the `PreToolUse` matcher.
    Write,
    /// `Read`. Never stored in the table; used to probe it for the Low tier.
    Read,
}

/// One thread's intent to write a file.
#[derive(Debug, Clone)]
pub struct Claim {
    /// Who.
    pub thread: ThreadId,
    /// What, as a logical path — so two worktrees collide on one key, which is
    /// the entire point of PRD §7.6.
    pub path: LogicalPath,
    /// Which physical checkout, which decides Medium versus High severity.
    pub worktree: WorktreeId,
    /// Branch name, or `None` when detached.
    ///
    /// Two claims both reporting `HEAD` are **not** necessarily on the same
    /// branch (4 463 records in the local corpus carry `gitBranch = "HEAD"`), so
    /// a detached head must not be tiered as "same branch" (ADR-0018).
    pub branch: Option<String>,
    /// Line range, when known. Absent for every subagent edit the transcript
    /// reports, which is why it is an `Option` rather than a field.
    pub lines: Option<(u32, u32)>,
    /// Whether this claim writes or reads.
    pub kind: ClaimKind,
    /// The worker inside the thread, or `None` for the main agent.
    ///
    /// Half of the key: together with [`Claim::thread`] it is the [`Actor`] the
    /// table collides on, so two workers of one session writing one file **is**
    /// a hit. See this module's header for the measurements that moved it from
    /// a label into the key.
    pub worker: Option<WorkerId>,
    /// Whether the write has happened yet.
    ///
    /// A claim starts [`ClaimPhase::Pending`] and is moved to
    /// [`ClaimPhase::Landed`] by [`ClaimTable::landed`], which also restarts its
    /// TTL — the hazard begins when the bytes hit the disk.
    pub phase: ClaimPhase,
    /// When the claim was registered, or — once it has landed — when it landed.
    pub at: Instant,
}

impl Claim {
    /// A write claim with nothing known but who, what and when.
    pub fn write(thread: ThreadId, path: LogicalPath, at: Instant) -> Self {
        Self {
            thread,
            path,
            worktree: WorktreeId::PRIMARY,
            branch: None,
            lines: None,
            kind: ClaimKind::Write,
            worker: None,
            phase: ClaimPhase::Pending,
            at,
        }
    }

    /// The same, for a read probe.
    pub fn read(thread: ThreadId, path: LogicalPath, at: Instant) -> Self {
        Self {
            kind: ClaimKind::Read,
            ..Self::write(thread, path, at)
        }
    }

    /// Sets the checkout and branch (ADR-0018 handles the detached case).
    #[must_use]
    pub fn in_checkout(mut self, worktree: WorktreeId, branch: Option<&str>) -> Self {
        self.worktree = worktree;
        self.branch = branch.and_then(named_branch);
        self
    }

    /// Attaches a line range from a `structuredPatch` hunk.
    #[must_use]
    pub fn with_lines(mut self, start: u32, end: u32) -> Self {
        self.lines = Some((start.min(end), start.max(end)));
        self
    }

    /// Attaches the worker that made the claim.
    #[must_use]
    pub fn by_worker(mut self, worker: Option<WorkerId>) -> Self {
        self.worker = worker;
        self
    }

    /// Marks the claim as covering a write that has already landed.
    ///
    /// Used when a channel reports the *result* rather than the intent — a
    /// `structuredPatch` arrives with the outcome, so the claim it upgrades has
    /// already happened by the time Polis hears about it.
    #[must_use]
    pub fn already_landed(mut self) -> Self {
        self.phase = ClaimPhase::Landed;
        self
    }

    /// Whether the write this claim covers has landed.
    pub fn has_landed(&self) -> bool {
        self.phase == ClaimPhase::Landed
    }

    /// Who holds this claim — the key the table collides on.
    pub fn actor(&self) -> Actor {
        Actor {
            thread: self.thread.clone(),
            worker: self.worker.clone(),
        }
    }

    /// Whether two claims are on the same branch of the same checkout.
    ///
    /// A detached head is never "the same branch" as anything, including another
    /// detached head (ADR-0018).
    pub fn same_branch_as(&self, other: &Self) -> bool {
        if self.worktree != other.worktree {
            return false;
        }
        match (&self.branch, &other.branch) {
            (Some(a), Some(b)) => a == b,
            // Same checkout with no branch reported on either side: the same
            // working tree is the same working tree, whatever HEAD says.
            (None, None) => self.worktree == other.worktree,
            _ => false,
        }
    }
}

/// `"HEAD"` is a detached head, not a branch name (ADR-0018).
fn named_branch(raw: &str) -> Option<String> {
    if raw.is_empty() || raw == "HEAD" {
        None
    } else {
        Some(raw.to_owned())
    }
}

/// A hit between two claims.
///
/// > This is a **relation between two threads, not a property of one**, so it is
/// > drawn as a link joining them across the map, not a badge on a dot. It is
/// > also the only state that can pull the eye to two places at once. (PRD §11.2)
#[derive(Debug, Clone)]
pub struct Contention {
    /// The claim that was already live.
    pub incumbent: Claim,
    /// The claim that arrived.
    pub challenger: Claim,
    /// How bad it is.
    pub severity: Severity,
    /// Whether the severity had line ranges to work with, or had to degrade to
    /// file level (ADR-0004).
    pub precision: ContentionPrecision,
}

impl Contention {
    /// Classifies a pair.
    pub fn of(incumbent: Claim, challenger: Claim) -> Self {
        let severity = classify(&incumbent, &challenger);
        let precision = if incumbent.lines.is_some() && challenger.lines.is_some() {
            ContentionPrecision::LineRange
        } else {
            ContentionPrecision::FileLevel
        };
        Self {
            incumbent,
            challenger,
            severity,
            precision,
        }
    }

    /// The path both threads are claiming.
    pub fn path(&self) -> &LogicalPath {
        &self.incumbent.path
    }

    /// The two threads, in a stable order, so the renderer draws one link and
    /// not two.
    ///
    /// **Equal when the hit is inside one thread** — two of its workers. Use
    /// [`Contention::actors`] to tell the two ends apart in that case, and
    /// [`Contention::is_within_thread`] to know before asking.
    pub fn threads(&self) -> (&ThreadId, &ThreadId) {
        if self.incumbent.thread <= self.challenger.thread {
            (&self.incumbent.thread, &self.challenger.thread)
        } else {
            (&self.challenger.thread, &self.incumbent.thread)
        }
    }

    /// The two actors, in a stable order. Always distinct — that is what made
    /// this a hit.
    pub fn actors(&self) -> (Actor, Actor) {
        let (a, b) = (self.incumbent.actor(), self.challenger.actor());
        if a <= b {
            (a, b)
        } else {
            (b, a)
        }
    }

    /// Whether both claimants belong to one thread — two workers of one session.
    ///
    /// Not a lesser state: same session means same checkout and same branch, so
    /// this is `High` or `Critical` by construction. It is exposed because a
    /// surface that says *"two agents"* should be able to say *"two workers of
    /// one agent"* instead, and because a link between a thread and itself has
    /// to be drawn from the two workers rather than from the thread twice.
    pub fn is_within_thread(&self) -> bool {
        self.incumbent.thread == self.challenger.thread
    }

    /// The two claims, in the same stable order as [`Contention::actors`].
    ///
    /// Each one names its own thread *and* its own worker, which is what a
    /// surface needs to place the two ends of the link separately when both
    /// belong to one session.
    pub fn ends(&self) -> (&Claim, &Claim) {
        if self.incumbent.actor() <= self.challenger.actor() {
            (&self.incumbent, &self.challenger)
        } else {
            (&self.challenger, &self.incumbent)
        }
    }

    /// The two ends resolved to map positions (PRD §11.2c).
    ///
    /// > This is a **relation between two threads, not a property of one**, so
    /// > it is drawn as a link joining them across the map, not a badge on a
    /// > dot.
    ///
    /// `find` is asked for each end's thread; it may return `None` for a thread
    /// the caller no longer holds, and the site of the collision stands in.
    ///
    /// # Two workers of one session
    ///
    /// Each end is placed by [`crate::place::agent_position`], which puts a
    /// *worker* at its own focus rather than at its thread's centre of mass — so
    /// two workers of one session are two positions, and the link joins them.
    /// [`ContentionLink::is_degenerate`] says when they nevertheless coincide,
    /// which is the honest outcome when both agents are working on exactly the
    /// one file they are fighting over: there is one place, and the surface must
    /// say so rather than draw a link to a position Polis invented.
    pub fn link<'a>(
        &self,
        layout: &polis_layout::CityLayout,
        mut find: impl FnMut(&ThreadId) -> Option<&'a crate::Thread>,
    ) -> ContentionLink {
        let (a, b) = self.ends();
        let site = crate::place::position_in(layout, self.path());
        let place_end = |claim: &Claim, thread: Option<&crate::Thread>| {
            thread
                .and_then(|t| crate::place::agent_position(t, claim.worker.as_ref(), layout))
                .or(site)
        };
        let at_a = place_end(a, find(&a.thread));
        let at_b = place_end(b, find(&b.thread));
        ContentionLink {
            a: a.actor(),
            b: b.actor(),
            at_a,
            at_b,
            site,
            severity: self.severity,
            precision: self.precision,
            within_thread: self.is_within_thread(),
        }
    }
}

/// The two ends of a contention, ready to draw (PRD §11.2c).
///
/// Built by [`Contention::link`]. It exists because the renderer previously had
/// only [`Contention::threads`] to work with, and two workers of one session
/// give that the *same* id twice — which draws a link of zero length, which is
/// to say a badge on a dot, which is the one thing PRD §11.2 says contention
/// must never be.
#[derive(Debug, Clone)]
pub struct ContentionLink {
    /// One end's claimant.
    pub a: Actor,
    /// The other's.
    pub b: Actor,
    /// Where `a` is, when the city has geometry for it.
    pub at_a: Option<polis_layout::Point>,
    /// Where `b` is.
    pub at_b: Option<polis_layout::Point>,
    /// The contended building itself, which both ends fall back to.
    pub site: Option<polis_layout::Point>,
    /// How bad it is.
    pub severity: Severity,
    /// Whether the tier had line ranges to work with.
    pub precision: ContentionPrecision,
    /// Whether both claimants are workers of one session.
    pub within_thread: bool,
}

impl ContentionLink {
    /// How far apart two positions must be to be two places, in city units.
    ///
    /// Below this the link is a dot and the surface should say *"two agents
    /// here"* rather than draw a line nobody can see.
    pub const MIN_SEPARATION: f32 = 1.0;

    /// Whether the two ends are the same place.
    ///
    /// True when Polis knows only one position — both agents working on the
    /// file they are fighting over, or a thread it no longer holds. Not a
    /// failure: it is the difference between *"these two are in different parts
    /// of the city"* and *"these two are on top of each other"*, and the second
    /// is the more alarming of the two.
    pub fn is_degenerate(&self) -> bool {
        match (self.at_a, self.at_b) {
            (Some(a), Some(b)) => {
                let dx = a.x - b.x;
                let dy = a.y - b.y;
                dx.hypot(dy) < Self::MIN_SEPARATION
            }
            _ => true,
        }
    }
}

/// Whether a severity tier was decided with line ranges or without them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentionPrecision {
    /// Both claims carried a `structuredPatch` line range. The Critical tier is
    /// reachable.
    LineRange,
    /// At least one claim had no line range — always the case for a subagent
    /// edit — so the classification stops at [`Severity::High`] ("same file")
    /// rather than guessing at overlap.
    FileLevel,
}

/// The severity tiers of PRD §11.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// One writing while another reads — a stale read.
    Low,
    /// Different worktrees or branches, same logical file. A merge conflict you
    /// will meet later.
    Medium,
    /// Same branch, same file, disjoint ranges.
    High,
    /// Same branch, overlapping line ranges. **Work is being clobbered now** —
    /// the only state where work is actively being destroyed, which is why
    /// PRD §11.1 orders contention above everything else.
    Critical,
}

impl Severity {
    /// A short operator-facing label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "stale read",
            Self::Medium => "merge conflict later",
            Self::High => "same file",
            Self::Critical => "clobbering now",
        }
    }
}

/// Classifies a pair of claims.
///
/// When either claim has no line range — always the case for a subagent edit —
/// the classification degrades to at most [`Severity::High`] rather than
/// guessing at overlap.
pub fn classify(incumbent: &Claim, challenger: &Claim) -> Severity {
    // "One writing while another reads" is its own tier and outranks nothing.
    if incumbent.kind == ClaimKind::Read || challenger.kind == ClaimKind::Read {
        return Severity::Low;
    }
    if !incumbent.same_branch_as(challenger) {
        return Severity::Medium;
    }
    match (incumbent.lines, challenger.lines) {
        (Some((a0, a1)), Some((b0, b1))) if a0 <= b1 && b0 <= a1 => Severity::Critical,
        // Disjoint ranges, or no range at all. Both are "same branch, same
        // file": with no range Polis does not know *where*, and saying
        // "overlapping" anyway would fabricate the one tier that means work is
        // being destroyed right now (ADR-0004).
        _ => Severity::High,
    }
}

/// Territory overlap — the early-warning form of contention (PRD §11.3).
///
/// > Two clouds overlapping means two orchestrators are claiming the same
/// > district, and it fires *before* anyone collides — while redirecting one is
/// > still cheap. Surface it as a distinct, quieter signal than file-level
/// > contention.
///
/// Overlap is field addition, so this is the **normalised inner product** of the
/// two Gaussian fields: `<a,b> / sqrt(<a,a>·<b,b>)`, where
/// `<x,y> = Σ wi·wj·exp(-d² / (ri² + rj²))` over kernel pairs. `1.0` is "these
/// two are the same cloud" — exactly, by construction, because a field's
/// normalised product with itself is one — and `None` is "they do not
/// meaningfully touch", because a quieter signal has to be able to stay quiet.
///
/// # The normalisation is the whole function
///
/// The previous form divided the cross term by the lighter field's mass
/// *squared* and then by the heavier field's mass again, which is `mass³` in a
/// quantity whose numerator scales as `mass²`. The consequence is not a tuning
/// question: two territories with **identical kernels** — the same four
/// buildings, the same district, which is the exact state this signal exists to
/// report — scored `0.033` against a `0.05` threshold and returned `None`. The
/// early warning could not fire at all, and nothing caught it because nothing
/// called it.
///
/// A ratio of masses is dimensionless and bounded by Cauchy-Schwarz, so the
/// documented meaning of `1.0` is now true rather than aspirational.
pub fn territory_overlap(
    a: &crate::territory::Territory,
    b: &crate::territory::Territory,
) -> Option<f32> {
    overlap_of(&CloudSummary::of(a)?, &CloudSummary::of(b)?)
}

/// How many kernels of a field the overlap product uses.
///
/// **Measured.** [`crate::territory::OBSERVATION_WINDOW`] lets a busy territory
/// hold 128 kernels, and the product is quadratic in them — 16 384 exponentials
/// for one pair. `World::refresh_overlaps` is quadratic in *threads* on top of
/// that, so PRD §16's hundred threads is 4 950 pairs, and one `tick` measured
/// **18.7 ms against PRD §13.1's 16.6 ms frame**: the early warning cost more
/// than the frame it was warning inside.
///
/// The heaviest eight kernels carry the shape of a cloud; the tail is decayed
/// evidence contributing a rounding error to a weighted product. Truncating
/// both sides *and* both self-terms keeps the quantity a normalised inner
/// product of the summarised fields, so `1.0` still means "the same cloud"
/// exactly.
pub const OVERLAP_KERNELS: usize = 8;

/// One cloud, reduced to what the overlap product needs.
///
/// Built once per thread per tick rather than once per *pair*: the self-term
/// `<a,a>` does not depend on `b`, and recomputing it inside the pair loop was
/// two thirds of the work.
#[derive(Debug, Clone)]
pub struct CloudSummary {
    /// The heaviest [`OVERLAP_KERNELS`] kernels, weight-descending.
    kernels: Vec<crate::territory::Kernel>,
    /// `<a,a>` over those kernels.
    self_product: f32,
    /// Centre of mass, for the cheap rejection.
    centre: Option<polis_layout::Point>,
    /// Widest kernel radius, likewise.
    reach: f32,
}

impl CloudSummary {
    /// Summarises a territory, or `None` when it has no cloud.
    ///
    /// Until a territory is placed there is nothing to overlap (PRD §6.2, §6.4),
    /// and that is a property of the evidence rather than of the geometry — so
    /// it is checked here, once, rather than in every pairing.
    ///
    /// The test is [`Territory::placement`], not the claim field, and the
    /// difference is the whole signal. PRD §11.3 is about *"two orchestrators
    /// claiming the same district"*, and an orchestrator is exactly the thread
    /// §6.2's single-ancestor gate rejects: its work is spread, so its trimmed
    /// ancestor collapses to the root and it has lobes instead of a claim. A
    /// claim-only test therefore made the early warning blind to the one kind
    /// of thread it was written about. Measured live on `qurio-toolset` with
    /// eight real sessions: five territories drawn, 106 000 of the cloud
    /// layer's 219 000 banded texels claimed by two or more of them — the map
    /// cross-hatching contested ground the overlap list could not name a pair
    /// for, because two of those five had lobes and no claim.
    ///
    /// [`Territory::placement`]: crate::territory::Territory::placement
    pub fn of(t: &crate::territory::Territory) -> Option<Self> {
        if !t.placement().is_somewhere() || t.kernels.is_empty() {
            return None;
        }
        let mut kernels = t.kernels.clone();
        // Descending by weight, then by position, so the summary of one field
        // does not depend on the order the kernels happen to be stored in
        // (PRD §7.4).
        kernels.sort_by(|x, y| {
            y.weight
                .partial_cmp(&x.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(
                    x.centre
                        .x
                        .partial_cmp(&y.centre.x)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
                .then(
                    x.centre
                        .y
                        .partial_cmp(&y.centre.y)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        });
        kernels.truncate(OVERLAP_KERNELS);
        let reach = kernels.iter().map(|k| k.radius).fold(0.0_f32, f32::max);
        let self_product = field_product(&kernels, &kernels);
        Some(Self {
            kernels,
            self_product,
            centre: t.centre_of_mass,
            reach,
        })
    }
}

/// The overlap of two already-summarised clouds.
///
/// See [`territory_overlap`] for what the number means; this is the form
/// `World::refresh_overlaps` uses, so a hundred threads pay for a hundred
/// summaries rather than for 9 900.
pub fn overlap_of(a: &CloudSummary, b: &CloudSummary) -> Option<f32> {
    // Cheap rejection before the quadratic part: at four sigma a Gaussian is
    // under 1e-7, so two fields that far apart cannot reach any threshold worth
    // having.
    if let (Some(ca), Some(cb)) = (a.centre, b.centre) {
        if (ca.x - cb.x).hypot(ca.y - cb.y) > 4.0 * (a.reach + b.reach) {
            return None;
        }
    }
    let cross = field_product(&a.kernels, &b.kernels);
    if cross <= 0.0 {
        return None;
    }
    // `<= 0.0 || !is_finite` rather than `!= 0.0`, so a NaN from a degenerate
    // kernel is "these do not touch" rather than a score nobody can order.
    let norm = (a.self_product * b.self_product).sqrt();
    if norm <= 0.0 || !norm.is_finite() {
        return None;
    }
    let score = (cross / norm).clamp(0.0, 1.0);
    (score > OVERLAP_THRESHOLD).then_some(score)
}

/// `<x,y> = Σ wi·wj·exp(-d² / (ri² + rj²))` over every kernel pair.
fn field_product(x: &[crate::territory::Kernel], y: &[crate::territory::Kernel]) -> f32 {
    let mut sum = 0.0_f32;
    for kx in x {
        for ky in y {
            let scale = kx.radius.mul_add(kx.radius, ky.radius * ky.radius);
            if scale <= 0.0 {
                continue;
            }
            let dx = kx.centre.x - ky.centre.x;
            let dy = kx.centre.y - ky.centre.y;
            sum += kx.weight * ky.weight * (-(dx * dx + dy * dy) / scale).exp();
        }
    }
    sum
}

/// Below this, two clouds are near each other rather than on top of each other.
///
/// PRD §17 lists cloud tuning as an open question; this is the one knob the
/// early-warning signal has, and it lives here rather than in the renderer.
pub const OVERLAP_THRESHOLD: f32 = 0.05;

/// Most overlaps carried at once, worst first.
///
/// The pairing is quadratic in threads, so a fleet of a hundred is 4 950 pairs.
/// The signal is *"which districts have two orchestrators in them"*, and past a
/// handful it has stopped answering that — PRD §17's test again.
pub const MAX_OVERLAPS: usize = 16;

/// Two orchestrators claiming one district — contention's early warning
/// (PRD §11.3).
///
/// > Two clouds overlapping means two orchestrators are claiming the same
/// > district, and it fires *before* anyone collides — while redirecting one is
/// > still cheap. **Surface it as a distinct, quieter signal than file-level
/// > contention.**
///
/// # Why this is not an `AttentionKind`
///
/// Because "quieter" is a claim about the contrast budget, not about wording.
/// PRD §10.3 gives the attention layer channels 169-255 and PRD §11.1 puts
/// contention at the top of that layer *because work is actively being
/// destroyed*. Nothing is being destroyed here — that is the entire point of an
/// early warning — so an overlap that entered the attention list would compete
/// for the eye with the state it is meant to help the operator avoid, and a map
/// where every near-miss shouts as loudly as a real clobber teaches the operator
/// to ignore both.
///
/// So overlaps live in their own list ([`crate::World::overlaps`]), carry a
/// [`TerritoryOverlap::score`] rather than a [`Severity`], and belong in the
/// cloud band (49-96) where the clouds they are about already are.
#[derive(Debug, Clone)]
pub struct TerritoryOverlap {
    /// One thread, the lower id of the pair.
    pub a: ThreadId,
    /// The other.
    pub b: ThreadId,
    /// The district `a` has converged on.
    pub claim_a: LogicalPath,
    /// The district `b` has converged on.
    pub claim_b: LogicalPath,
    /// How much the two fields share, in `(0, 1]`. `1.0` is "the same cloud".
    pub score: f32,
    /// When this pair was first seen overlapping, so a surface can fade it in
    /// rather than flashing it — a quiet signal that blinks is not quiet.
    pub since: Instant,
}

impl TerritoryOverlap {
    /// Whether the two orchestrators have converged on the *same* district, as
    /// opposed to two districts whose clouds merely bleed into each other.
    ///
    /// The stronger of the two readings, and the one worth a sentence in a
    /// drill-down: they are not near each other, they are in the same place.
    pub fn same_district(&self) -> bool {
        self.claim_a == self.claim_b
    }

    /// Stable pair key, so a surface can tell one overlap from another across
    /// frames without depending on iteration order.
    pub fn key(&self) -> (&ThreadId, &ThreadId) {
        (&self.a, &self.b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_events::SessionId;

    fn thread(name: &str) -> ThreadId {
        ThreadId::of_session(SessionId::new(name))
    }

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    /// Two buildings a measurable distance apart, so a link between two agents
    /// has two ends to join.
    fn two_building_city() -> polis_layout::CityLayout {
        use polis_layout::{Building, LotId, Point, Polygon, RoofForm};
        let mut layout = polis_layout::CityLayout {
            extent: 100.0,
            ..polis_layout::CityLayout::default()
        };
        for (i, name) in ["src/a.rs", "src/b.rs"].into_iter().enumerate() {
            #[allow(clippy::cast_precision_loss)] // two, for a test
            let x = i as f32 * 40.0;
            layout.buildings.insert(
                lp(name),
                Building {
                    path: lp(name),
                    lot: LotId(u32::try_from(i).unwrap()),
                    footprint: Polygon::new(vec![
                        Point::new(x, 0.0),
                        Point::new(x + 2.0, 0.0),
                        Point::new(x + 2.0, 2.0),
                        Point::new(x, 2.0),
                    ]),
                    height: 1.0,
                    roof: RoofForm::Flat,
                    rotation: 0.0,
                },
            );
        }
        layout
    }

    #[test]
    fn a_second_live_claim_on_the_same_logical_path_is_a_hit() {
        let mut table = ClaimTable::default();
        let t0 = Instant::now();
        assert!(table
            .claim(Claim::write(thread("a"), lp("src/auth.ts"), t0))
            .is_none());
        let hit = table
            .claim(Claim::write(thread("b"), lp("src/auth.ts"), t0))
            .expect("a second live claim is a hit");
        assert_eq!(hit.severity, Severity::High);
        assert_eq!(hit.precision, ContentionPrecision::FileLevel);
        assert_eq!(table.total_hits(), 1);
    }

    #[test]
    fn two_worktrees_collide_on_one_key_and_tier_medium() {
        // PRD §7.6: `/repo-wt-3/src/auth.ts` and `/repo-wt-7/src/auth.ts` are the
        // same logical file, and that collision is the point.
        let mut table = ClaimTable::default();
        let t0 = Instant::now();
        table.claim(
            Claim::write(thread("a"), lp("src/auth.ts"), t0)
                .in_checkout(WorktreeId(3), Some("feature-a")),
        );
        let hit = table
            .claim(
                Claim::write(thread("b"), lp("src/auth.ts"), t0)
                    .in_checkout(WorktreeId(7), Some("feature-b")),
            )
            .expect("one logical file, two checkouts");
        assert_eq!(
            hit.severity,
            Severity::Medium,
            "a merge conflict you will meet later"
        );
    }

    #[test]
    fn a_detached_head_is_not_a_branch() {
        // ADR-0018: 4 463 records carry gitBranch = "HEAD".
        let t0 = Instant::now();
        let a = Claim::write(thread("a"), lp("src/x.rs"), t0)
            .in_checkout(WorktreeId(1), Some("HEAD"))
            .with_lines(10, 20);
        let b = Claim::write(thread("b"), lp("src/x.rs"), t0)
            .in_checkout(WorktreeId(2), Some("HEAD"))
            .with_lines(15, 25);
        assert!(a.branch.is_none() && b.branch.is_none());
        assert_eq!(
            classify(&a, &b),
            Severity::Medium,
            "two detached heads are not the same branch"
        );
    }

    #[test]
    fn overlapping_line_ranges_on_one_branch_are_critical() {
        let t0 = Instant::now();
        let a = Claim::write(thread("a"), lp("src/x.rs"), t0)
            .in_checkout(WorktreeId::PRIMARY, Some("main"))
            .with_lines(10, 20);
        let b = Claim::write(thread("b"), lp("src/x.rs"), t0)
            .in_checkout(WorktreeId::PRIMARY, Some("main"))
            .with_lines(18, 30);
        assert_eq!(classify(&a, &b), Severity::Critical);
        let disjoint = Claim::write(thread("c"), lp("src/x.rs"), t0)
            .in_checkout(WorktreeId::PRIMARY, Some("main"))
            .with_lines(40, 50);
        assert_eq!(classify(&a, &disjoint), Severity::High);
    }

    #[test]
    fn a_missing_line_range_degrades_to_same_file_and_says_so() {
        // ADR-0004: `structuredPatch` is absent on 65% of subagent tool results,
        // including 625 `Edit` calls. Guessing Critical there would poison the
        // one state where work is actively being destroyed.
        let t0 = Instant::now();
        let with = Claim::write(thread("a"), lp("src/x.rs"), t0)
            .in_checkout(WorktreeId::PRIMARY, Some("main"))
            .with_lines(10, 20);
        let without = Claim::write(thread("b"), lp("src/x.rs"), t0)
            .in_checkout(WorktreeId::PRIMARY, Some("main"));
        assert_eq!(classify(&with, &without), Severity::High);
        let hit = Contention::of(with, without);
        assert_eq!(hit.precision, ContentionPrecision::FileLevel);
        assert!(hit.severity < Severity::Critical);
    }

    #[test]
    fn one_writing_while_another_reads_is_low() {
        let mut table = ClaimTable::default();
        let t0 = Instant::now();
        table.claim(Claim::write(thread("a"), lp("src/x.rs"), t0));
        let hit = table
            .note_read(&Claim::read(thread("b"), lp("src/x.rs"), t0))
            .expect("a stale read");
        assert_eq!(hit.severity, Severity::Low);
        // The probe registers nothing.
        assert_eq!(table.claims_on(&lp("src/x.rs")).len(), 1);
        // And a thread reading its own write is not contention.
        assert!(table
            .note_read(&Claim::read(thread("a"), lp("src/x.rs"), t0))
            .is_none());

        // Once the write has landed the file is just a file: the reader gets
        // the new bytes, and calling that a stale read would fabricate the tier.
        table.landed(&Actor::main(thread("a")), &lp("src/x.rs"), t0);
        assert!(
            table
                .note_read(&Claim::read(thread("b"), lp("src/x.rs"), t0))
                .is_none(),
            "\"one writing while another reads\" is about a write in flight"
        );
    }

    #[test]
    fn a_landed_write_keeps_contending_and_an_abandoned_one_does_not() {
        // The measured defect: replaying the operator's two real sessions with
        // claims keyed by actor still produced **zero** contentions, because a
        // transcript's `tool_result` lands under a second after its `tool_use`
        // and the claim was deleted there. Two workers 4.7 s apart never held a
        // claim at the same instant.
        let mut table = ClaimTable::default();
        let t0 = Instant::now();
        let a = Actor::worker(thread("s"), WorkerId::new("w1"));
        let b = Actor::worker(thread("s"), WorkerId::new("w2"));

        table.claim(
            Claim::write(thread("s"), lp("src/x.rs"), t0).by_worker(Some(WorkerId::new("w1"))),
        );
        // The result arrives half a second later, as it does in every transcript.
        table.landed(&a, &lp("src/x.rs"), t0 + Duration::from_millis(500));
        assert!(table.claims_on(&lp("src/x.rs"))[0].has_landed());

        // And 4.7 s later a sibling worker writes the same file. That is the
        // clobber, and it must be a hit.
        let hit = table
            .claim(
                Claim::write(
                    thread("s"),
                    lp("src/x.rs"),
                    t0 + Duration::from_millis(4_700),
                )
                .by_worker(Some(WorkerId::new("w2"))),
            )
            .expect("the settings.css case, in miniature");
        assert!(hit.is_within_thread());
        assert_eq!(hit.actors(), (a.clone(), b));

        // A write that never landed is the case early release is for.
        table.claim(
            Claim::write(thread("s"), lp("src/y.rs"), t0).by_worker(Some(WorkerId::new("w1"))),
        );
        table.release(&a, &lp("src/y.rs"));
        assert!(
            table.claims_on(&lp("src/y.rs")).is_empty(),
            "nothing was written, so there is nothing to write over"
        );
    }

    #[test]
    fn a_landing_restarts_the_ttl_and_never_moves_a_claim_backwards() {
        let mut table = ClaimTable::default();
        let t0 = Instant::now();
        let a = Actor::main(thread("a"));
        table.claim(Claim::write(thread("a"), lp("src/x.rs"), t0));

        // The hazard begins when the bytes reach the disk, so the TTL runs from
        // there — 20 s after the call, the claim has 30 s left, not 10.
        table.landed(&a, &lp("src/x.rs"), t0 + Duration::from_secs(20));
        table.expire(t0 + Duration::from_secs(45));
        assert_eq!(table.len(), 1, "25 s after the landing, still live");
        table.expire(t0 + Duration::from_secs(51));
        assert!(table.is_empty(), "31 s after it, gone");

        // Channel A exports every 5 s, so a landing can be reported twice and
        // out of order. The later report must not un-age the claim.
        table.claim(Claim::write(thread("a"), lp("src/z.rs"), t0));
        table.landed(&a, &lp("src/z.rs"), t0 + Duration::from_secs(10));
        table.landed(&a, &lp("src/z.rs"), t0 + Duration::from_secs(2));
        table.expire(t0 + Duration::from_secs(39));
        assert_eq!(
            table.len(),
            1,
            "the later landing is the one that counts: 29 s after it, still live"
        );
        table.expire(t0 + Duration::from_secs(41));
        assert!(table.is_empty(), "and 31 s after it, gone");
    }

    #[test]
    fn the_table_is_bounded_by_paths_as_well_as_by_claimants() {
        // A landed claim holds its path for 30 s, so the number of paths is
        // bounded only by how fast a fleet writes — and `hits` walks all of them
        // inside PRD §13.1's frame budget.
        let mut table = ClaimTable::default();
        let t0 = Instant::now();
        for i in 0..(MAX_CLAIMED_PATHS + 64) {
            table.claim(Claim::write(
                thread("a"),
                lp(&format!("src/f{i}.rs")),
                t0 + Duration::from_millis(u64::try_from(i).unwrap()),
            ));
        }
        assert_eq!(table.len(), MAX_CLAIMED_PATHS);
        assert_eq!(table.paths_evicted(), 64, "counted, not hidden");
        assert!(
            table.claims_on(&lp("src/f0.rs")).is_empty(),
            "the stalest path is the one that goes"
        );
        assert!(
            !table
                .claims_on(&lp(&format!("src/f{}.rs", MAX_CLAIMED_PATHS + 63)))
                .is_empty(),
            "and never the write that just happened"
        );
    }

    #[test]
    fn one_thread_editing_repeatedly_refreshes_rather_than_contends() {
        let mut table = ClaimTable::default();
        let t0 = Instant::now();
        table.claim(Claim::write(thread("a"), lp("src/x.rs"), t0));
        assert!(table
            .claim(Claim::write(
                thread("a"),
                lp("src/x.rs"),
                t0 + Duration::from_secs(5)
            ))
            .is_none());
        assert_eq!(table.claims_on(&lp("src/x.rs")).len(), 1);
    }

    #[test]
    fn claims_expire_at_the_ttl_and_release_is_early() {
        let mut table = ClaimTable::default();
        let t0 = Instant::now();
        table.claim(Claim::write(thread("a"), lp("src/x.rs"), t0));
        table.expire(t0 + CLAIM_TTL.saturating_sub(Duration::from_secs(1)));
        assert_eq!(table.len(), 1);
        table.expire(t0 + CLAIM_TTL + Duration::from_secs(1));
        assert!(table.is_empty(), "the 30 s TTL is the backstop");

        table.claim(Claim::write(thread("a"), lp("src/y.rs"), t0));
        table.release(&Actor::main(thread("a")), &lp("src/y.rs"));
        assert!(table.is_empty(), "and an abandoned write releases early");
    }

    #[test]
    fn hits_are_recomputed_from_live_claims_and_ordered_worst_first() {
        let mut table = ClaimTable::default();
        let t0 = Instant::now();
        table.claim(
            Claim::write(thread("a"), lp("src/x.rs"), t0)
                .in_checkout(WorktreeId::PRIMARY, Some("main"))
                .with_lines(1, 10),
        );
        table.claim(
            Claim::write(thread("b"), lp("src/x.rs"), t0)
                .in_checkout(WorktreeId::PRIMARY, Some("main"))
                .with_lines(5, 15),
        );
        table.claim(Claim::write(thread("c"), lp("src/z.rs"), t0).in_checkout(WorktreeId(2), None));
        table.claim(Claim::write(thread("d"), lp("src/z.rs"), t0).in_checkout(WorktreeId(3), None));

        let hits = table.hits(t0);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].severity, Severity::Critical);
        assert_eq!(hits[1].severity, Severity::Medium);

        // Past the TTL the relation stops existing, which is what makes the
        // link disappear rather than linger.
        assert!(table.hits(t0 + CLAIM_TTL * 2).is_empty());
    }

    #[test]
    fn the_thread_pair_has_a_stable_order() {
        let t0 = Instant::now();
        let a = Claim::write(thread("aaa"), lp("x"), t0);
        let b = Claim::write(thread("bbb"), lp("x"), t0);
        let one = Contention::of(a.clone(), b.clone());
        let two = Contention::of(b, a);
        assert_eq!(one.threads(), two.threads());
    }

    #[test]
    fn overlapping_territories_are_the_early_warning_and_distant_ones_are_quiet() {
        use crate::territory::Territory;
        use crate::{Observation, PathScope};
        use polis_events::ToolKind;
        use polis_layout::Point;

        let now = Instant::now();
        let build = |dir: &str, x: f32| {
            let mut t = Territory::for_extent(1000.0);
            for i in 0..6 {
                let path = LogicalPath::new(&format!("{dir}/f{i}.rs")).unwrap();
                t.observe(
                    &Observation {
                        thread: thread("t"),
                        worker: None,
                        scope: PathScope::File,
                        path,
                        tool: ToolKind::Edit,
                        at: now,
                        weight: None,
                    },
                    1.0,
                    Some(Point::new(x, 0.0)),
                );
            }
            t
        };
        let a = build("src/auth", 0.0);
        let b = build("src/auth2", 1.0);
        let far = build("docs/deep", 100_000.0);
        assert!(
            territory_overlap(&a, &b).is_some(),
            "two orchestrators on one district fire early"
        );
        assert!(
            territory_overlap(&a, &far).is_none(),
            "and a quiet signal stays quiet"
        );
        // An unconverged territory has no cloud, so it cannot overlap one.
        assert!(territory_overlap(&a, &Territory::default()).is_none());

        // The state the signal exists to report: two orchestrators whose
        // kernels are the *same buildings*. Under the old `mass³`
        // normalisation this scored 0.033 against a 0.05 threshold and was
        // silently `None` — the early warning could not fire at all.
        let same = territory_overlap(&a, &a).expect("a cloud overlaps itself");
        assert!(
            (same - 1.0).abs() < 1e-3,
            "a field's normalised product with itself is one, which is what \
             makes 1.0 mean \"the same cloud\": {same}"
        );
        let near = territory_overlap(&a, &b).unwrap();
        assert!(
            near > OVERLAP_THRESHOLD && near <= 1.0,
            "and a neighbouring district is somewhere between: {near}"
        );

        // The truncation to `OVERLAP_KERNELS` must not cost the identity: a
        // field summarised to its heaviest eight kernels is still normalised
        // against *its own* summary, so `1.0` keeps meaning "the same cloud"
        // however many kernels the territory holds.
        let mut wide = Territory::for_extent(1000.0);
        for i in 0..(OVERLAP_KERNELS * 4) {
            let path = LogicalPath::new(&format!("src/wide/f{i}.rs")).unwrap();
            #[allow(clippy::cast_precision_loss)] // a test's kernel scatter
            let x = i as f32 * 0.1;
            wide.observe(
                &Observation {
                    thread: thread("t"),
                    worker: None,
                    scope: PathScope::File,
                    path,
                    tool: ToolKind::Edit,
                    at: now,
                    weight: None,
                },
                1.0,
                Some(Point::new(x, 0.0)),
            );
        }
        assert!(wide.kernels.len() > OVERLAP_KERNELS);
        let self_wide = territory_overlap(&wide, &wide).expect("a wide cloud overlaps itself");
        assert!(
            (self_wide - 1.0).abs() < 1e-3,
            "truncation must be applied to both sides and to the normaliser: {self_wide}"
        );
        assert_eq!(
            CloudSummary::of(&wide).unwrap().kernels.len(),
            OVERLAP_KERNELS
        );
    }

    /// PRD §11.3 is about *"two orchestrators claiming the same district"*, and
    /// an orchestrator is precisely the thread PRD §6.2's single-ancestor gate
    /// rejects — its work is spread, so its trimmed ancestor is the repository
    /// root and it has PRD §6.4's lobes instead of a claim. A `claim.is_none()`
    /// filter therefore excluded the early warning's own subject.
    #[test]
    fn an_orchestrator_with_lobes_and_no_claim_still_has_a_cloud_to_overlap() {
        use crate::territory::Territory;
        use crate::{Observation, PathScope};
        use polis_events::ToolKind;
        use polis_layout::Point;

        let now = Instant::now();
        let mut spread = Territory::for_extent(1000.0);
        // Interleaved: PRD §6.3 contracts slowly, so a thread that works in one
        // directory before it fans out keeps that first claim. A real
        // orchestrator's workers report from everywhere at once.
        let places = ["src/components/a.jsx", "src/hooks/b.js", "tests/c.test.js"];
        for k in 0..12 {
            for (i, path) in places.iter().enumerate() {
                #[allow(clippy::cast_precision_loss)] // a test's kernel scatter
                let x = (k * 3 + i) as f32 * 0.1;
                spread.observe(
                    &Observation {
                        thread: thread("orchestrator"),
                        worker: None,
                        scope: PathScope::File,
                        path: lp(path),
                        tool: ToolKind::Read,
                        at: now,
                        weight: None,
                    },
                    1.0,
                    Some(Point::new(x, 0.0)),
                );
            }
        }
        assert!(
            spread.claim.is_none(),
            "the fixture must be the case §6.2 cannot name: {:?}",
            spread.claim
        );
        assert!(!spread.lobes.is_empty(), "but §6.4 can");
        assert!(
            CloudSummary::of(&spread).is_some(),
            "a lobed thread has a cloud, so it has something to overlap"
        );
        let same = territory_overlap(&spread, &spread)
            .expect("and the early warning can therefore see it at all");
        assert!((same - 1.0).abs() < 1e-3, "same cloud is 1.0: {same}");
    }

    #[test]
    fn an_overlap_names_the_district_and_says_when_two_agents_are_in_the_same_one() {
        let now = Instant::now();
        let one = TerritoryOverlap {
            a: thread("a"),
            b: thread("b"),
            claim_a: lp("src/auth"),
            claim_b: lp("src/auth"),
            score: 0.8,
            since: now,
        };
        let near = TerritoryOverlap {
            claim_b: lp("src/render"),
            ..one.clone()
        };
        assert!(
            one.same_district(),
            "two orchestrators claiming one district is the stronger reading"
        );
        assert!(
            !near.same_district(),
            "adjacent clouds are not the same claim"
        );
        assert_eq!(one.key(), (&thread("a"), &thread("b")));
    }

    #[test]
    fn a_link_between_two_workers_of_one_session_has_two_ends() {
        // PRD §11.2c: contention is *"a relation between two threads, not a
        // property of one, so it is drawn as a link joining them across the map,
        // not a badge on a dot"*. `threads()` gives one id twice when both
        // claimants are workers of one session, and a link of zero length is a
        // badge on a dot — which is the one thing §11.2 says it must not be.
        use crate::{Thread, Worker};
        use polis_events::SessionId;

        let t0 = Instant::now();
        let layout = two_building_city();
        let mut owner = Thread::new(thread("s"), SessionId::new("s"), t0);
        for (id, focus) in [("w1", "src/a.rs"), ("w2", "src/b.rs")] {
            let mut w = Worker::new(WorkerId::new(id), t0);
            w.focus = Some(lp(focus));
            owner.workers.push(w);
        }

        let path = lp("src/a.rs");
        let hit = Contention::of(
            Claim::write(thread("s"), path.clone(), t0).by_worker(Some(WorkerId::new("w1"))),
            Claim::write(thread("s"), path, t0).by_worker(Some(WorkerId::new("w2"))),
        );
        assert!(hit.is_within_thread());
        assert_eq!(hit.threads().0, hit.threads().1, "one id, twice");

        let link = hit.link(&layout, |id| (id == &thread("s")).then_some(&owner));
        assert!(link.within_thread);
        assert_ne!(link.a, link.b, "but two actors");
        assert!(
            !link.is_degenerate(),
            "and two places, because each worker is placed at its own focus: \
             {:?} vs {:?}",
            link.at_a,
            link.at_b
        );
        assert_eq!(link.severity, hit.severity);
    }

    #[test]
    fn two_agents_on_top_of_each_other_are_one_place_and_the_model_says_so() {
        use crate::{Thread, Worker};
        use polis_events::SessionId;

        let t0 = Instant::now();
        let layout = two_building_city();
        let mut owner = Thread::new(thread("s"), SessionId::new("s"), t0);
        // Both workers focused on the file they are fighting over, which is the
        // common shape. There is one place, and inventing a second would be a
        // lie about where an agent is.
        for id in ["w1", "w2"] {
            let mut w = Worker::new(WorkerId::new(id), t0);
            w.focus = Some(lp("src/a.rs"));
            owner.workers.push(w);
        }
        let path = lp("src/a.rs");
        let hit = Contention::of(
            Claim::write(thread("s"), path.clone(), t0).by_worker(Some(WorkerId::new("w1"))),
            Claim::write(thread("s"), path, t0).by_worker(Some(WorkerId::new("w2"))),
        );
        let link = hit.link(&layout, |id| (id == &thread("s")).then_some(&owner));
        assert!(
            link.is_degenerate(),
            "one place, honestly reported: {:?} vs {:?}",
            link.at_a,
            link.at_b
        );
        assert_eq!(link.at_a, link.site, "and the place is the contended file");
    }

    #[test]
    fn a_link_falls_back_to_the_contended_file_when_a_thread_is_gone() {
        let t0 = Instant::now();
        let layout = two_building_city();
        let hit = Contention::of(
            Claim::write(thread("a"), lp("src/a.rs"), t0),
            Claim::write(thread("b"), lp("src/a.rs"), t0),
        );
        let link = hit.link(&layout, |_| None);
        assert!(link.site.is_some());
        assert_eq!(link.at_a, link.site);
        assert_eq!(link.at_b, link.site);
        assert!(
            link.is_degenerate(),
            "the operator still has to be shown where the collision is"
        );
    }
}
