//! Growth order from git history (PRD §7.1).
//!
//! > **`git log` is the growth order.** Replay it in commit order. Files added in
//! > the repo's first year form the old town — dense, tangled, irregular. Files
//! > added last month sit on the periphery and look more planned. This is not
//! > decoration: the age structure of the codebase becomes visible at a glance.
//!
//! Bootstrap command, verbatim from the PRD:
//!
//! ```text
//! git log --diff-filter=A --name-only --reverse --format=%H|%ct
//! ```
//!
//! Invoked as a subprocess rather than through libgit2: the output is one stable
//! plumbing format, and it avoids a C toolchain on every platform for a job that
//! runs once per `HEAD`.

use std::path::Path;

use polis_events::{LogicalPath, WorktreeId};

/// The order in which files first appeared, oldest first.
///
/// Cached keyed on `HEAD` and recomputed incrementally on new commits.
#[derive(Debug, Clone, Default)]
pub struct GrowthSequence {
    /// `(path, commit timestamp)` in commit order. The index into this vector is
    /// `FileFacts::growth_index`.
    pub entries: Vec<(LogicalPath, i64)>,
    /// The `HEAD` this was derived from. A mismatch invalidates the cache.
    pub head: String,
}

impl GrowthSequence {
    /// Runs the bootstrap command and parses its output.
    pub fn bootstrap(repo_root: &Path) -> anyhow::Result<Self> {
        let _ = repo_root;
        todo!("PRD §7.1 — git log --diff-filter=A --name-only --reverse --format=%H|%ct")
    }

    /// Extends a cached sequence with commits made since `self.head`.
    pub fn extend_to_head(&mut self, repo_root: &Path) -> anyhow::Result<usize> {
        let _ = repo_root;
        todo!("PRD §7.4 — incremental, never a full recompute")
    }
}

/// Enumerates `git worktree list --porcelain` (PRD §7.6).
///
/// This, `SessionStart`'s `cwd`, and `CwdChanged` are the three ways Polis
/// learns about a worktree. Never the `WorktreeCreate` hook, which Polis must not
/// register (ADR-0002).
pub fn list_worktrees(repo_root: &Path) -> anyhow::Result<Vec<Worktree>> {
    let _ = repo_root;
    todo!("PRD §7.6 — git worktree list --porcelain")
}

/// One checkout of the repository.
#[derive(Debug, Clone)]
pub struct Worktree {
    /// Stable id used as a rendering dimension, not as part of the layout key.
    pub id: WorktreeId,
    /// Absolute path of this checkout.
    pub path: std::path::PathBuf,
    /// Branch name, or `None` when detached.
    ///
    /// 4 463 records in the local corpus carry `gitBranch = "HEAD"`. Two records
    /// both reading `HEAD` are **not** necessarily on the same branch, so
    /// contention severity must not treat detached heads as equal
    /// (PRD §11.3, ADR-0018).
    pub branch: Option<String>,
}

/// Uncommitted diff line counts per file — PRD §7.3's building height.
///
/// The transcript already carries this three ways, so this is the *fallback*,
/// not the primary source: `structuredPatch` gives exact ±counts per edit,
/// `toolUseResult.toolStats` gives a per-subagent roll-up, and `cost-state`
/// gives a per-session total. `structuredPatch` is missing for subagent edits,
/// which is why the fallback exists at all (ADR-0004).
pub fn diff_line_counts(repo_root: &Path) -> anyhow::Result<Vec<(LogicalPath, u32)>> {
    let _ = repo_root;
    todo!("PRD §7.3 — git diff --numstat against HEAD")
}
