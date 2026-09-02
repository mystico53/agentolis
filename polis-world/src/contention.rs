//! Contention detection (PRD §11.3).
//!
//! > On `PreToolUse` for `Edit`/`Write`, register a claim on the **logical path**
//! > keyed by thread, with a 30s TTL. A second live claim on the same logical
//! > path is a hit.
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

use polis_events::{LogicalPath, ThreadId, WorktreeId};

/// How long a claim stays live (PRD §11.3).
pub const CLAIM_TTL: Duration = Duration::from_secs(30);

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
}

impl Default for ClaimTable {
    fn default() -> Self {
        Self {
            live: BTreeMap::new(),
            ttl: CLAIM_TTL,
            total_hits: 0,
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
    /// A second live claim on the same logical path from a **different** thread
    /// is a hit; a second claim from the same thread refreshes the first, which
    /// is what a thread editing one file repeatedly looks like.
    ///
    /// When more than one other thread holds the path, the worst pairing is
    /// returned — the operator needs the severity, and the drill-down layer can
    /// ask [`ClaimTable::claims_on`] for the rest.
    pub fn claim(&mut self, claim: Claim) -> Option<Contention> {
        let ttl = self.ttl;
        let entry = self.live.entry(claim.path.clone()).or_default();
        entry.retain(|c| claim.at.saturating_duration_since(c.at) <= ttl);

        let worst = entry
            .iter()
            .filter(|c| c.thread != claim.thread)
            .map(|incumbent| Contention::of(incumbent.clone(), claim.clone()))
            .max_by_key(|c| c.severity);

        if let Some(slot) = entry.iter_mut().find(|c| c.thread == claim.thread) {
            *slot = claim;
        } else {
            entry.push(claim);
        }
        if worst.is_some() {
            self.total_hits += 1;
        }
        worst
    }

    /// Probes the table with a read, without registering anything.
    ///
    /// This is PRD §11.3's Low tier — "one writing while another reads" — and it
    /// is a probe rather than a claim because registering every `Read` would
    /// make the table the ~200 calls/sec firehose PRD §4.2 exists to avoid.
    pub fn note_read(&self, reader: &Claim) -> Option<Contention> {
        let ttl = self.ttl;
        self.live
            .get(&reader.path)?
            .iter()
            .filter(|c| c.thread != reader.thread && c.kind == ClaimKind::Write)
            .filter(|c| reader.at.saturating_duration_since(c.at) <= ttl)
            .map(|incumbent| Contention::of(incumbent.clone(), reader.clone()))
            .max_by_key(|c| c.severity)
    }

    /// Releases a claim early, when the matching write is observed landing.
    ///
    /// The trigger is Channel A's `tool_result` / `claude_code.tool` span or
    /// Channel C's `Modified`, **not** a `PostToolUse` hook: Polis does not
    /// register one, because a matched `PostToolUse` costs a process spawn on
    /// every write for a signal the 30 s TTL and Channel A already carry
    /// (ADR-0044). Logs export every 5 s by default, so a released-late claim is
    /// normal and the TTL remains the backstop.
    pub fn release(&mut self, thread: &ThreadId, path: &LogicalPath) {
        if let Some(entry) = self.live.get_mut(path) {
            entry.retain(|c| &c.thread != thread);
            if entry.is_empty() {
                self.live.remove(path);
            }
        }
    }

    /// Expires claims older than [`CLAIM_TTL`].
    pub fn expire(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.live.retain(|_, entry| {
            entry.retain(|c| now.saturating_duration_since(c.at) <= ttl);
            !entry.is_empty()
        });
    }

    /// Every live contention, worst first.
    ///
    /// One entry per `(path, incumbent, challenger)` triple. Contention is a
    /// relation, so this is recomputed rather than remembered: a mark that
    /// outlived its pair of claims would be a red link between two threads that
    /// are no longer colliding.
    pub fn hits(&self, now: Instant) -> Vec<Contention> {
        let ttl = self.ttl;
        let mut out = Vec::new();
        for entry in self.live.values() {
            let live: Vec<&Claim> = entry
                .iter()
                .filter(|c| now.saturating_duration_since(c.at) <= ttl)
                .collect();
            for (i, incumbent) in live.iter().enumerate() {
                for challenger in live.iter().skip(i + 1) {
                    if incumbent.thread == challenger.thread {
                        continue;
                    }
                    // The older claim is the incumbent, whichever order the
                    // table happens to hold them in.
                    let (a, b) = if incumbent.at <= challenger.at {
                        ((*incumbent).clone(), (*challenger).clone())
                    } else {
                        ((*challenger).clone(), (*incumbent).clone())
                    };
                    out.push(Contention::of(a, b));
                }
            }
        }
        out.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(a.incumbent.path.cmp(&b.incumbent.path))
                .then(a.incumbent.thread.cmp(&b.incumbent.thread))
                .then(a.challenger.thread.cmp(&b.challenger.thread))
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
    /// The worker inside the thread, when the claim came from a subagent. Two
    /// workers of the *same* thread editing one file is coordination, not
    /// contention, so this never raises a hit — but the drill-down layer needs
    /// it to name who.
    pub worker: Option<polis_events::WorkerId>,
    /// When the claim was registered.
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
    pub fn by_worker(mut self, worker: Option<polis_events::WorkerId>) -> Self {
        self.worker = worker;
        self
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
    pub fn threads(&self) -> (&ThreadId, &ThreadId) {
        if self.incumbent.thread <= self.challenger.thread {
            (&self.incumbent.thread, &self.challenger.thread)
        } else {
            (&self.challenger.thread, &self.incumbent.thread)
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
/// Overlap is field addition, so this is the normalised cross term of the two
/// Gaussian fields: `Σ wa·wb·exp(-d² / (ra² + rb²))` over kernel pairs, divided
/// by the lighter field's mass. `1.0` is "these two are the same cloud", and
/// `None` is "they do not meaningfully touch" — a quieter signal has to be able
/// to stay quiet.
pub fn territory_overlap(
    a: &crate::territory::Territory,
    b: &crate::territory::Territory,
) -> Option<f32> {
    if a.claim.is_none() || b.claim.is_none() {
        // Until a territory converges there is no cloud to overlap (PRD §6.2).
        return None;
    }
    let mass = a.mass().min(b.mass());
    if mass <= 0.0 {
        return None;
    }
    let mut cross = 0.0_f32;
    for ka in &a.kernels {
        for kb in &b.kernels {
            let dx = ka.centre.x - kb.centre.x;
            let dy = ka.centre.y - kb.centre.y;
            let scale = ka.radius.mul_add(ka.radius, kb.radius * kb.radius);
            if scale <= 0.0 {
                continue;
            }
            cross += ka.weight * kb.weight * (-(dx * dx + dy * dy) / scale).exp();
        }
    }
    let score = cross / (mass * mass.max(1.0)).max(f32::EPSILON);
    let score = (score / a.mass().max(b.mass()).max(1.0)).min(1.0);
    (score > OVERLAP_THRESHOLD).then_some(score)
}

/// Below this, two clouds are near each other rather than on top of each other.
///
/// PRD §17 lists cloud tuning as an open question; this is the one knob the
/// early-warning signal has, and it lives here rather than in the renderer.
pub const OVERLAP_THRESHOLD: f32 = 0.05;

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
        table.release(&thread("a"), &lp("src/y.rs"));
        assert!(table.is_empty(), "and the write landing releases early");
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
    }
}
