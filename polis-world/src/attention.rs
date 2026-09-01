//! The attention layer (PRD §11.1, §11.2, §11.4).
//!
//! > **This is the first milestone that delivers the actual product thesis.**
//! > (PRD §15, M5)
//!
//! # Ordering
//!
//! `contention > needs-decision > done`. Contention is the only one where work
//! is actively being destroyed. A pending decision costs wall clock. Done costs
//! nothing.
//!
//! # Peripheral perception (PRD §11.4)
//!
//! Peripheral vision is poor at colour and good at motion onset, so the
//! **arrival** of a mark is a brief pulse (≤400 ms) and its **steady state** is
//! shape and position. Colour alone is never the sole channel for any state —
//! which is why [`Attention`] carries a kind and a position, and the renderer
//! derives colour from them rather than the reverse.

use std::time::{Duration, Instant};

use polis_events::{LogicalPath, ThreadId};

use crate::contention::Contention;

/// How long an arrival pulse lasts (PRD §11.4).
pub const PULSE: Duration = Duration::from_millis(400);

/// How long "done, verified" holds full weight before decaying (PRD §11.2).
pub const DONE_VERIFIED_FULL_WEIGHT: Duration = Duration::from_secs(20);

/// One mark on the attention layer, which owns the top of the contrast range
/// (PRD §10.3).
#[derive(Debug, Clone)]
pub struct Attention {
    /// Which of the three states.
    pub kind: AttentionKind,
    /// When it arrived. Drives the ≤400 ms pulse and any decay.
    pub since: Instant,
}

/// Exactly three operator-facing states (PRD §11.2).
#[derive(Debug, Clone)]
pub enum AttentionKind {
    /// **(a) Needs decision** — persistent, amber, drawn as a standing pin above
    /// the building or district. Persists until resolved.
    ///
    /// > This is the primary state; it is what the product is for.
    ///
    /// Sources: `PermissionRequest`, `Elicitation`, `TeammateIdle`, and the
    /// `permission_prompt` / `idle_prompt` / `agent_needs_input` notification
    /// types, which fire even with desktop notifications disabled.
    NeedsDecision {
        /// The thread waiting on a human.
        thread: ThreadId,
        /// Where to point, when the request names a file.
        at: Option<LogicalPath>,
        /// Which signal raised it, for the drill-down layer.
        source: DecisionSource,
    },
    /// **(b) Done** — teal, decaying. **Main agents only.**
    ///
    /// > `SubagentStop` is noise; a main thread going idle is news.
    ///
    /// Split by verification, because without the split `done` floods the
    /// display within the hour and buries state (a).
    Done {
        /// The thread that finished.
        thread: ThreadId,
        /// Whether tests ran against the changed files **after** the change.
        ///
        /// `false` means this is really "needs review", and it **persists**
        /// rather than decaying — the second most important thing on the map.
        /// PRD §17 open question 2 asks whether it should be a fourth state
        /// outright; it behaves more like `NeedsDecision` than like `Done`.
        verified: bool,
    },
    /// **(c) Contention** — red. A relation between two threads, drawn as a link
    /// joining them across the map, never a badge on a dot.
    Contention(Box<Contention>),
}

/// Which signal raised a "needs decision" mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionSource {
    /// `PermissionRequest`. Note that auto-mode denials are invisible here and
    /// arrive as `PermissionDenied` instead.
    PermissionRequest,
    /// `Elicitation` — an MCP server asking the user for input mid-tool-call.
    Elicitation,
    /// `TeammateIdle`.
    TeammateIdle,
    /// A `Notification` of type `permission_prompt`, `idle_prompt` or
    /// `agent_needs_input`.
    Notification,
}

impl AttentionKind {
    /// Sort key implementing PRD §11.1's ordering. Lower sorts first.
    pub fn rank(&self) -> u8 {
        match self {
            Self::Contention(_) => 0,
            Self::NeedsDecision { .. } => 1,
            Self::Done { .. } => 2,
        }
    }

    /// Whether the mark decays or persists until resolved.
    ///
    /// Only `Done { verified: true }` decays. `NeedsDecision` persists until
    /// resolved, contention persists while the claims overlap, and
    /// `Done { verified: false }` persists because it is really "needs review".
    pub fn decays(&self) -> bool {
        matches!(self, Self::Done { verified: true, .. })
    }
}

/// Re-sorts the attention list into PRD §11.1 order.
///
/// Ties break on arrival time, oldest first, so the list does not reshuffle
/// under the operator's eye while they are reading it.
pub fn sort(marks: &mut [Attention]) {
    let _ = marks;
    todo!("PRD §11.1 — contention > needs-decision > done, then oldest first")
}

/// Drops marks whose decay has completed.
pub fn expire(marks: &mut Vec<Attention>, now: Instant) {
    let _ = (marks, now);
    todo!("PRD §11.2 — done/verified decays after 20 s; nothing else expires on a timer")
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_events::SessionId;

    fn thread() -> ThreadId {
        ThreadId::of_session(SessionId::new("s"))
    }

    #[test]
    fn ordering_puts_destruction_first() {
        // PRD §11.1: contention is the only state where work is actively being
        // destroyed, so it outranks everything regardless of arrival time.
        let decision = AttentionKind::NeedsDecision {
            thread: thread(),
            at: None,
            source: DecisionSource::PermissionRequest,
        };
        let done = AttentionKind::Done {
            thread: thread(),
            verified: true,
        };
        assert!(decision.rank() < done.rank());
    }

    #[test]
    fn only_verified_done_decays() {
        // Without this split, `done` floods the display within the hour and
        // buries the state the product exists for.
        assert!(AttentionKind::Done {
            thread: thread(),
            verified: true
        }
        .decays());
        assert!(!AttentionKind::Done {
            thread: thread(),
            verified: false
        }
        .decays());
        assert!(!AttentionKind::NeedsDecision {
            thread: thread(),
            at: None,
            source: DecisionSource::Elicitation,
        }
        .decays());
    }
}
