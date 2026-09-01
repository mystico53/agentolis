//! Channel C — the filesystem watcher (PRD §4.3).
//!
//! > Completely out of band; zero agent impact. Gives you the ground truth of
//! > what actually changed on disk, which OTel does not.
//!
//! # Attribution
//!
//! The filesystem does not know which agent wrote. Correlate write events
//! against the tool-call stream within a ±2 s window keyed on logical path —
//! but anchor that window on the **hook or OTel clock**, which is stamped at the
//! moment of the call, never on a transcript timestamp: 20% of transcript files
//! contain a backwards step and one observed jump was 60 seconds (ADR-0014).
//!
//! Where attribution must be certain, PRD §4.3's `FileChanged` fallback does not
//! exist. That hook watches a literal, explicitly named filename list, and its
//! payload carries no `tool_name` and no `tool_use_id` — it is exactly as
//! attribution-blind as this channel while additionally costing a process spawn.
//! `PreToolUse` claims are the only authoritative channel (ADR-0003).
//!
//! # Ignores
//!
//! `.git/`, `node_modules/`, `target/`, `dist/`, and anything in `.gitignore`.

use std::path::Path;

use polis_events::PathMapper;

use crate::bus::EventSink;

/// The `notify` watcher thread.
#[derive(Debug)]
pub struct FsWatcher {
    _private: (),
}

impl FsWatcher {
    /// Starts watching a repository root recursively.
    ///
    /// Watch every registered worktree, not only the primary: PRD §7.6 makes
    /// worktrees first-class, and the whole point is seeing two agents edit the
    /// same logical file on different branches.
    pub fn start(roots: &[&Path], sink: EventSink, mapper: PathMapper) -> anyhow::Result<Self> {
        let _ = (roots, sink, mapper);
        todo!("PRD §4.3 — notify RecommendedWatcher, recursive, with the ignore set")
    }

    /// True once the watcher has reported a queue overflow and the tree needs a
    /// re-scan. Ignoring overflow silently desynchronises the city from disk.
    pub fn needs_rescan(&self) -> bool {
        todo!("PRD §4.3 — surface notify's rescan signal, do not swallow it")
    }
}

/// Whether a path is excluded from the city (PRD §4.3, §8 "Industrial zone").
///
/// `node_modules`, vendored and generated trees are *not* simply ignored: they
/// are rendered as a single dull mass. This predicate governs the **watch**, and
/// a separate classifier in `polis-repo` governs the rendering.
pub fn is_watch_excluded(path: &Path) -> bool {
    let _ = path;
    todo!("PRD §4.3 — .git/, node_modules/, target/, dist/, plus .gitignore")
}

/// A Windows-specific startup self-check.
///
/// A deep `cwd` produces a 282-character transcript path on a machine with
/// `LongPathsEnabled = 0`. Rust's `std::fs` copes (verified); `notify` against
/// such a directory is **untested**. PRD §16 should not discover this at
/// runtime, so startup actually reads a discovered path before advertising the
/// session as watchable.
pub fn self_check_long_paths(sample: &Path) -> bool {
    let _ = sample;
    todo!("docs/verified/jsonl-schema.md §1 — read a real long path before advertising")
}
