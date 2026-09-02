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
//!
//! [`Attention::pulse`] and [`Attention::weight`] are the two numbers a renderer
//! needs: the first is the onset, the second is the steady-state prominence.
//! Both are computed here so every surface agrees on when a mark stops being
//! urgent.

use std::time::{Duration, Instant};

use polis_events::{LogicalPath, ThreadId};

use crate::contention::Contention;

/// How long an arrival pulse lasts (PRD §11.4).
pub const PULSE: Duration = Duration::from_millis(400);

/// How long "done, verified" holds full weight before decaying (PRD §11.2).
pub const DONE_VERIFIED_FULL_WEIGHT: Duration = Duration::from_secs(20);

/// How long "done, verified" takes to fade to the base layer once its full
/// weight has elapsed.
///
/// > *Done, verified* (tests ran against the changed files after the change):
/// > full weight 20s, then decays to base layer.
///
/// The PRD fixes the 20 s and leaves the ramp open; a matching 20 s ramp keeps
/// the whole mark inside a minute, which is what stops `done` flooding the
/// display within the hour.
pub const DONE_VERIFIED_DECAY: Duration = Duration::from_secs(20);

/// One mark on the attention layer, which owns the top of the contrast range
/// (PRD §10.3).
#[derive(Debug, Clone)]
pub struct Attention {
    /// Which of the three states.
    pub kind: AttentionKind,
    /// When it arrived. Drives the ≤400 ms pulse and any decay.
    pub since: Instant,
}

impl Attention {
    /// A mark arriving now.
    pub fn new(kind: AttentionKind, at: Instant) -> Self {
        Self { kind, since: at }
    }

    /// The arrival pulse, `1.0` at onset falling to `0.0` at [`PULSE`]
    /// (PRD §11.4).
    ///
    /// > The **arrival** of an attention mark is a brief pulse (≤400ms). That is
    /// > what catches the eye when the operator is not looking at the screen.
    pub fn pulse(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.since);
        if elapsed >= PULSE {
            return 0.0;
        }
        1.0 - elapsed.as_secs_f32() / PULSE.as_secs_f32()
    }

    /// Steady-state prominence in `[0, 1]`.
    ///
    /// `1.0` for everything that persists — needs-decision, contention, and
    /// *done, unverified*, which is really "needs review". Only *done, verified*
    /// decays, and only after its full-weight window.
    pub fn weight(&self, now: Instant) -> f32 {
        if !self.kind.decays() {
            return 1.0;
        }
        let elapsed = now.saturating_duration_since(self.since);
        if elapsed <= DONE_VERIFIED_FULL_WEIGHT {
            return 1.0;
        }
        let into_decay = elapsed.saturating_sub(DONE_VERIFIED_FULL_WEIGHT);
        if into_decay >= DONE_VERIFIED_DECAY {
            return 0.0;
        }
        1.0 - into_decay.as_secs_f32() / DONE_VERIFIED_DECAY.as_secs_f32()
    }

    /// Whether the mark has decayed to nothing and should be dropped.
    pub fn is_spent(&self, now: Instant) -> bool {
        self.kind.decays() && self.weight(now) <= 0.0
    }

    /// The thread this mark is about, for the follow-thread camera. Contention
    /// answers with its incumbent, since it is about two.
    pub fn thread(&self) -> &ThreadId {
        match &self.kind {
            AttentionKind::NeedsDecision { thread, .. } | AttentionKind::Done { thread, .. } => {
                thread
            }
            AttentionKind::Contention(c) => &c.incumbent.thread,
        }
    }
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

impl DecisionSource {
    /// An operator-facing label for the status rail.
    pub fn label(self) -> &'static str {
        match self {
            Self::PermissionRequest => "permission",
            Self::Elicitation => "elicitation",
            Self::TeammateIdle => "teammate idle",
            Self::Notification => "notification",
        }
    }
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

    /// Whether two marks are about the same thing, so the second replaces the
    /// first instead of stacking.
    ///
    /// A thread that asks for the same permission twice is one pin, not two; a
    /// thread that finishes and is then verified updates in place.
    pub fn same_subject(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::NeedsDecision {
                    thread: a,
                    source: sa,
                    ..
                },
                Self::NeedsDecision {
                    thread: b,
                    source: sb,
                    ..
                },
            ) => a == b && sa == sb,
            (Self::Done { thread: a, .. }, Self::Done { thread: b, .. }) => a == b,
            (Self::Contention(a), Self::Contention(b)) => {
                a.threads() == b.threads() && a.path() == b.path()
            }
            _ => false,
        }
    }

    /// Whether this is a pending decision belonging to `thread`.
    pub fn is_decision_for(&self, thread: &ThreadId) -> bool {
        matches!(self, Self::NeedsDecision { thread: t, .. } if t == thread)
    }

    /// The building or district the mark points at, when it names one.
    pub fn at(&self) -> Option<&LogicalPath> {
        match self {
            Self::NeedsDecision { at, .. } => at.as_ref(),
            Self::Contention(c) => Some(c.path()),
            Self::Done { .. } => None,
        }
    }
}

/// Re-sorts the attention list into PRD §11.1 order.
///
/// Ties break on arrival time, oldest first, so the list does not reshuffle
/// under the operator's eye while they are reading it.
pub fn sort(marks: &mut [Attention]) {
    marks.sort_by(|a, b| {
        a.kind
            .rank()
            .cmp(&b.kind.rank())
            // Within contention, worse first: Critical is the only state where
            // work is being destroyed right now.
            .then_with(|| contention_severity(b).cmp(&contention_severity(a)))
            .then(a.since.cmp(&b.since))
    });
}

/// Contention severity as a sort key; everything else sorts equal.
fn contention_severity(mark: &Attention) -> Option<crate::contention::Severity> {
    match &mark.kind {
        AttentionKind::Contention(c) => Some(c.severity),
        _ => None,
    }
}

/// Drops marks whose decay has completed.
pub fn expire(marks: &mut Vec<Attention>, now: Instant) {
    marks.retain(|m| !m.is_spent(now));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contention::{Claim, ClaimKind, Severity};
    use polis_events::{SessionId, WorktreeId};

    fn thread() -> ThreadId {
        ThreadId::of_session(SessionId::new("s"))
    }

    fn other() -> ThreadId {
        ThreadId::of_session(SessionId::new("t"))
    }

    fn contention(severity_driver: Option<(u32, u32)>) -> Contention {
        let at = Instant::now();
        let path = LogicalPath::new("src/x.rs").unwrap();
        let mut a =
            Claim::write(thread(), path.clone(), at).in_checkout(WorktreeId(0), Some("main"));
        let mut b = Claim::write(other(), path, at).in_checkout(WorktreeId(0), Some("main"));
        if let Some((s, e)) = severity_driver {
            a = a.with_lines(s, e);
            b = b.with_lines(s, e);
        }
        assert_eq!(a.kind, ClaimKind::Write);
        Contention::of(a, b)
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

    #[test]
    fn sort_is_contention_then_decision_then_done_and_stable_on_age() {
        let t0 = Instant::now();
        let mut marks = vec![
            Attention::new(
                AttentionKind::Done {
                    thread: thread(),
                    verified: true,
                },
                t0,
            ),
            Attention::new(
                AttentionKind::NeedsDecision {
                    thread: other(),
                    at: None,
                    source: DecisionSource::Notification,
                },
                t0 + Duration::from_secs(5),
            ),
            Attention::new(
                AttentionKind::NeedsDecision {
                    thread: thread(),
                    at: None,
                    source: DecisionSource::PermissionRequest,
                },
                t0 + Duration::from_secs(1),
            ),
            Attention::new(
                AttentionKind::Contention(Box::new(contention(None))),
                t0 + Duration::from_secs(9),
            ),
        ];
        sort(&mut marks);
        assert_eq!(marks[0].kind.rank(), 0);
        assert_eq!(marks[1].kind.rank(), 1);
        assert_eq!(marks[2].kind.rank(), 1);
        assert!(
            marks[1].since < marks[2].since,
            "oldest first inside a tier, so the list does not reshuffle while it is read"
        );
        assert_eq!(marks[3].kind.rank(), 2);
    }

    #[test]
    fn worse_contention_sorts_above_milder_contention() {
        let t0 = Instant::now();
        let mut marks = vec![
            Attention::new(AttentionKind::Contention(Box::new(contention(None))), t0),
            Attention::new(
                AttentionKind::Contention(Box::new(contention(Some((1, 9))))),
                t0 + Duration::from_secs(1),
            ),
        ];
        sort(&mut marks);
        match &marks[0].kind {
            AttentionKind::Contention(c) => assert_eq!(c.severity, Severity::Critical),
            other => panic!("expected contention first: {other:?}"),
        }
    }

    #[test]
    fn a_verified_done_holds_full_weight_then_fades_and_is_dropped() {
        let t0 = Instant::now();
        let mark = Attention::new(
            AttentionKind::Done {
                thread: thread(),
                verified: true,
            },
            t0,
        );
        assert!((mark.weight(t0) - 1.0).abs() < f32::EPSILON);
        assert!((mark.weight(t0 + DONE_VERIFIED_FULL_WEIGHT) - 1.0).abs() < f32::EPSILON);
        let mid = mark.weight(t0 + DONE_VERIFIED_FULL_WEIGHT + DONE_VERIFIED_DECAY / 2);
        assert!(mid > 0.4 && mid < 0.6, "halfway through the ramp: {mid}");
        assert!(mark.is_spent(t0 + DONE_VERIFIED_FULL_WEIGHT + DONE_VERIFIED_DECAY));

        let mut marks = vec![mark];
        expire(&mut marks, t0 + Duration::from_secs(1));
        assert_eq!(marks.len(), 1);
        expire(&mut marks, t0 + Duration::from_secs(120));
        assert!(marks.is_empty());
    }

    #[test]
    fn done_unverified_never_expires_because_it_is_needs_review() {
        let t0 = Instant::now();
        let mut marks = vec![Attention::new(
            AttentionKind::Done {
                thread: thread(),
                verified: false,
            },
            t0,
        )];
        expire(&mut marks, t0 + Duration::from_hours(24));
        assert_eq!(marks.len(), 1, "the second most important thing on the map");
        assert!((marks[0].weight(t0 + Duration::from_hours(24)) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn the_arrival_pulse_is_short_and_then_gone() {
        let t0 = Instant::now();
        let mark = Attention::new(
            AttentionKind::NeedsDecision {
                thread: thread(),
                at: None,
                source: DecisionSource::PermissionRequest,
            },
            t0,
        );
        assert!((mark.pulse(t0) - 1.0).abs() < f32::EPSILON);
        assert!(mark.pulse(t0 + PULSE / 2) > 0.4);
        assert!((mark.pulse(t0 + PULSE)).abs() < f32::EPSILON);
        assert!((mark.pulse(t0 + Duration::from_secs(5))).abs() < f32::EPSILON);
    }

    #[test]
    fn the_same_subject_replaces_rather_than_stacks() {
        let a = AttentionKind::NeedsDecision {
            thread: thread(),
            at: None,
            source: DecisionSource::PermissionRequest,
        };
        let b = AttentionKind::NeedsDecision {
            thread: thread(),
            at: Some(LogicalPath::new("src/x.rs").unwrap()),
            source: DecisionSource::PermissionRequest,
        };
        let different_source = AttentionKind::NeedsDecision {
            thread: thread(),
            at: None,
            source: DecisionSource::Elicitation,
        };
        assert!(a.same_subject(&b));
        assert!(!a.same_subject(&different_source));
        assert!(a.is_decision_for(&thread()));
        assert!(!a.is_decision_for(&other()));
    }
}
