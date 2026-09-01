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

use std::time::{Duration, Instant};

use polis_events::{LogicalPath, ThreadId, WorktreeId};

/// How long a claim stays live (PRD §11.3).
pub const CLAIM_TTL: Duration = Duration::from_secs(30);

/// Live claims on logical paths.
#[derive(Debug, Default)]
pub struct ClaimTable {
    _private: (),
}

impl ClaimTable {
    /// Registers a claim and returns any contention it creates.
    pub fn claim(&mut self, claim: Claim) -> Option<Contention> {
        // Taken by value because the table stores it. Dropped for now.
        drop(claim);
        todo!("PRD §11.3 — a second live claim on the same logical path is a hit")
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
        let _ = (thread, path);
        todo!("PRD §11.3 — early release; the 30 s TTL is the backstop")
    }

    /// Expires claims older than [`CLAIM_TTL`].
    pub fn expire(&mut self, now: Instant) {
        let _ = now;
        todo!("PRD §11.3 — 30 s TTL")
    }
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
    /// When the claim was registered.
    pub at: Instant,
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

/// Classifies a pair of claims.
///
/// When either claim has no line range — always the case for a subagent edit —
/// the classification degrades to at most [`Severity::High`] rather than
/// guessing at overlap.
pub fn classify(incumbent: &Claim, challenger: &Claim) -> Severity {
    let _ = (incumbent, challenger);
    todo!("PRD §11.3 — degrade to `same file` when line ranges are unavailable")
}

/// Territory overlap — the early-warning form of contention (PRD §11.3).
///
/// > Two clouds overlapping means two orchestrators are claiming the same
/// > district, and it fires *before* anyone collides — while redirecting one is
/// > still cheap. Surface it as a distinct, quieter signal than file-level
/// > contention.
pub fn territory_overlap(
    a: &crate::territory::Territory,
    b: &crate::territory::Territory,
) -> Option<f32> {
    let _ = (a, b);
    todo!("PRD §11.3 — field addition; quieter than a file-level hit")
}
