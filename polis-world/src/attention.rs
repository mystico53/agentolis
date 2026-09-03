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

/// How long a persistent mark takes to reach full [`Attention::urgency`].
///
/// # The number PRD §11.4 needs and does not state
///
/// §11.4 fixes the arrival at a ≤400 ms pulse and says the steady state is
/// "shape and position". Both are true and neither is the whole story: a pin
/// that arrived a second ago and a pin that has stood for half an hour are the
/// same shape in the same position, and only one of them means *the operator
/// has not noticed*. The pulse is over in 400 ms, so with nothing else the map
/// cannot distinguish them at all.
///
/// So a persistent mark has a second, much slower ramp, and its length comes
/// from the corpus rather than from taste. Measured over 1 440 sample points of
/// the operator's six largest real sessions (77 standing-pin observations,
/// `polis-world/tests/attention_states.rs`):
///
/// | percentile | standing age |
/// |---|---|
/// | p50 | 134 s |
/// | p75 | 942 s |
/// | p90 | 2 217 s |
/// | max | 21 210 s (5.9 h) |
///
/// 61 % of observed waits were already past 60 s, **35 % past 300 s** and 26 %
/// past 900 s. Five minutes is therefore the point where a wait stops being
/// ordinary: a median wait reaches urgency 0.45 and never saturates, and the
/// third of pins that are genuinely being forgotten do saturate, which is the
/// only population the escalation is for.
///
/// Applies to every mark that persists — contention and *done, unverified*
/// included — because "still here" is the same fact in all three cases.
pub const ESCALATION: Duration = Duration::from_secs(300);

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

    /// How long this mark has stood.
    pub fn waited(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.since)
    }

    /// Escalation in `[0, 1]`: `0` at arrival, `1` at [`ESCALATION`].
    ///
    /// The slow companion to [`Attention::pulse`]. The pulse is motion onset and
    /// is gone in 400 ms; this is the channel that says *nobody has dealt with
    /// this*, and a renderer spends it on **area** — a mark that has stood for
    /// five minutes is physically bigger than one that arrived a second ago, so
    /// the difference survives being seen from across the room and survives
    /// being seen in greyscale.
    ///
    /// Always `0` for a decaying mark: *done, verified* is already on its way
    /// out and growing it would contradict its own weight.
    pub fn urgency(&self, now: Instant) -> f32 {
        if self.kind.decays() {
            return 0.0;
        }
        let waited = self.waited(now).as_secs_f32();
        (waited / ESCALATION.as_secs_f32()).clamp(0.0, 1.0)
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
///
/// # The first four are Channel B; the last three are Channel D
///
/// PRD §11.2 sources this state from hooks, and a **replayed transcript carries
/// no hooks**: measured across all three M2 recordings, the attention band was
/// 0.000% of map area in every one of 1 440 frames, because nothing in a
/// transcript replay could raise a mark at all. A recording that can never show
/// the state the product exists for is not evidence that the state works.
///
/// So three sources are reconstructed from Channel D, and
/// [`DecisionSource::is_reconstructed`] says which. They are not equivalent to
/// the hooks and must not be presented as if they were — see
/// `docs/replay/README.md` for exactly what a replay can and cannot show.
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

    /// **Channel D.** The agent called a tool whose entire purpose is to ask the
    /// operator — `AskUserQuestion`, `ExitPlanMode` (see
    /// [`asks_the_operator`]).
    ///
    /// This one is *prospective and exact*: the `tool_use` block is the question
    /// and its `tool_result` is the answer, so the mark's onset and its duration
    /// are both the real ones. Measured in the operator's own corpus: seven such
    /// calls in session `29c2fc6f`, with waits from 42 s to 2 h.
    AskUser,
    /// **Channel D.** A main agent ended its turn with no tool call and no human
    /// has replied yet — the transcript's form of the `idle_prompt` /
    /// `agent_needs_input` notification PRD §11.2 lists under this state.
    ///
    /// Also prospective and exact, and by far the most common: 62 / 38 / 6 of
    /// them in the three recorded sessions, with median waits of 5, 13 and 27
    /// minutes. This is the reason a replay can show the primary state at all.
    TurnEnded,
    /// **Channel D.** A `tool_result` carrying `toolDenialKind`, or an
    /// `[Request interrupted by user]` record: proof that a permission prompt
    /// was shown *and answered*.
    ///
    /// The only **retrospective** source. Channel D has no record of the prompt
    /// itself, so the mark arrives with the answer rather than with the
    /// question — late by however long the operator took to decide. 20 of these
    /// in `29c2fc6f`, 8 in `6f51089f`.
    Rejected,
}

impl DecisionSource {
    /// An operator-facing label for the status rail.
    pub fn label(self) -> &'static str {
        match self {
            Self::PermissionRequest => "permission",
            Self::Elicitation => "elicitation",
            Self::TeammateIdle => "teammate idle",
            Self::Notification => "notification",
            Self::AskUser => "asked you",
            Self::TurnEnded => "waiting on you",
            Self::Rejected => "you said no",
        }
    }

    /// Whether this mark was reconstructed from a transcript rather than
    /// delivered by a hook.
    ///
    /// The distinction is not cosmetic: PRD §17 makes anything that drives an
    /// alert come from an authoritative channel, and a replayed transcript is
    /// not one. A live session raises the first four; a replay raises the last
    /// three and nothing else.
    pub fn is_reconstructed(self) -> bool {
        matches!(self, Self::AskUser | Self::TurnEnded | Self::Rejected)
    }

    /// Whether the mark's onset is the real one.
    ///
    /// False only for [`DecisionSource::Rejected`], whose evidence arrives with
    /// the operator's answer rather than with the question.
    pub fn onset_is_exact(self) -> bool {
        !matches!(self, Self::Rejected)
    }
}

/// Whether a tool call *is* a question to the operator.
///
/// `AskUserQuestion` and `ExitPlanMode` block on a human by construction: the
/// call cannot return until somebody answers it. That makes them the one
/// "waiting on you" signal a transcript carries **prospectively** — the mark can
/// be raised when the call is made, exactly as a `PermissionRequest` hook would
/// have raised it, with no lookahead and no guessing.
///
/// Matched on the wire name because neither is a [`polis_events::ToolKind`]
/// variant; both
/// parse to `ToolKind::Other`, and adding variants to that enum for a signal
/// that only the attention layer reads would put the vocabulary in the wrong
/// crate.
pub fn asks_the_operator(tool: &polis_events::ToolKind) -> bool {
    matches!(tool.name(), "AskUserQuestion" | "ExitPlanMode")
}

impl AttentionKind {
    /// Sort key implementing PRD §11.1's ordering. Lower sorts first.
    ///
    /// # PRD §17, open question 2, answered from the corpus
    ///
    /// > Should `done, unverified` be a fourth attention state rather than a
    /// > variant of `done`? It behaves more like `needs decision`.
    ///
    /// **No — but it gets its own rank, and it must not share `done`'s.**
    ///
    /// The question is one of frequency, so it was measured rather than argued
    /// (`polis-world/tests/attention_states.rs`, six real sessions, 1 440 sample
    /// points). At every sample the counterfactual `finish_thread` reads was
    /// asked of every thread that had written a file — *if this thread stopped
    /// now, which variant?*
    ///
    /// | session | raises `done, unverified` |
    /// |---|---|
    /// | `29c2fc6f` | 67.8 % |
    /// | `dfa8cd66` | 64.5 % |
    /// | `bdbc403b` | 58.9 % |
    /// | `e41f2794` | 20.4 % |
    /// | **all** | **52.7 %** (453 of 860 thread-samples) |
    ///
    /// So it is not an edge case, it is a coin flip: *"needs review" is the
    /// modal outcome of a session ending.* A variant that fires half the time
    /// cannot be distinguished by colour alone, cannot decay, and cannot sort
    /// underneath the variant that means "this one is finished".
    ///
    /// It is still not a fourth **state**, on three counts:
    ///
    /// * it is **bounded**. [`AttentionKind::same_subject`] folds `Done` by
    ///   thread, so a thread has at most one, ever. `needs decision` is bounded
    ///   the same way *per source* and contention is not bounded at all. The
    ///   flooding §11.2 warns about is a property of the decaying variant, and
    ///   the split already fixes it — measured peak, one at once per thread;
    /// * it **asks nothing**. §11.2a's state names a question with an answer the
    ///   operator owes; this one names work that exists and has not been
    ///   checked. Nothing unblocks when it is cleared, so it cannot go in the
    ///   tier whose whole meaning is "wall clock is being burned";
    /// * §11.1's ordering needs a **rank**, not a state. Ranking it above
    ///   `done, verified` is the entire behavioural difference a fourth state
    ///   would have bought.
    ///
    /// What it does get, because 52.7 % demands it, is a shape of its own and
    /// persistence — see `polis_render::live::MarkKind`.
    pub fn rank(&self) -> u8 {
        match self {
            Self::Contention(_) => 0,
            Self::NeedsDecision { .. } => 1,
            // "Needs review" outranks "finished": one is work nobody has looked
            // at, the other is work that checked itself.
            Self::Done {
                verified: false, ..
            } => 2,
            Self::Done { verified: true, .. } => 3,
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
            // Actors, not threads: two workers of one session colliding on one
            // file is a different mark from two *other* workers of that session
            // colliding on it, and folding them together would report one
            // clobber where there are two.
            (Self::Contention(a), Self::Contention(b)) => {
                a.actors() == b.actors() && a.path() == b.path()
            }
            _ => false,
        }
    }

    /// Whether this is a pending decision belonging to `thread`.
    pub fn is_decision_for(&self, thread: &ThreadId) -> bool {
        matches!(self, Self::NeedsDecision { thread: t, .. } if t == thread)
    }

    /// Whether the mark refers to `thread` at **either** end.
    ///
    /// Contention is a relation, so it names two threads and a mark that
    /// survived one of them being retired would point at nothing. Used by
    /// [`crate::World::forget_thread`], which is the only place a thread leaves
    /// the world.
    pub fn mentions_thread(&self, thread: &ThreadId) -> bool {
        match self {
            Self::NeedsDecision { thread: t, .. } | Self::Done { thread: t, .. } => t == thread,
            Self::Contention(c) => &c.incumbent.thread == thread || &c.challenger.thread == thread,
        }
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
        assert_eq!(marks[3].kind.rank(), 3);
    }

    /// PRD §17 q2's answer, as a test: "needs review" sorts above "finished".
    ///
    /// Measured at 52.7 % of thread-samples across six real sessions, so this is
    /// the majority variant and cannot sit underneath the minority one.
    #[test]
    fn needs_review_outranks_finished_and_both_stay_under_a_decision() {
        let unverified = AttentionKind::Done {
            thread: thread(),
            verified: false,
        };
        let verified = AttentionKind::Done {
            thread: thread(),
            verified: true,
        };
        let decision = AttentionKind::NeedsDecision {
            thread: thread(),
            at: None,
            source: DecisionSource::PermissionRequest,
        };
        assert!(unverified.rank() < verified.rank());
        assert!(decision.rank() < unverified.rank());
        // …and it is still one mark per thread, which is why it is a rank and
        // not a fourth state: it cannot accumulate.
        assert!(unverified.same_subject(&verified));
    }

    /// PRD §11.4's second channel: the pulse says *something arrived*, urgency
    /// says *nobody has dealt with it*. The first is over in 400 ms; the second
    /// is the only thing that separates a fresh pin from a forgotten one.
    #[test]
    fn a_standing_mark_escalates_over_five_minutes_and_a_decaying_one_never_does() {
        let t0 = Instant::now();
        let pin = Attention::new(
            AttentionKind::NeedsDecision {
                thread: thread(),
                at: None,
                source: DecisionSource::TurnEnded,
            },
            t0,
        );
        assert!(pin.urgency(t0).abs() < f32::EPSILON);
        // The measured median wait, 134 s: escalated but not saturated.
        let median = pin.urgency(t0 + Duration::from_secs(134));
        assert!(median > 0.4 && median < 0.5, "median wait: {median}");
        assert!((pin.urgency(t0 + ESCALATION) - 1.0).abs() < f32::EPSILON);
        assert!((pin.urgency(t0 + Duration::from_hours(6)) - 1.0).abs() < f32::EPSILON);
        assert_eq!(pin.waited(t0 + Duration::from_secs(7)).as_secs(), 7);

        // A mark already on its way out must not grow as it goes.
        let done = Attention::new(
            AttentionKind::Done {
                thread: thread(),
                verified: true,
            },
            t0,
        );
        assert!(done.urgency(t0 + ESCALATION).abs() < f32::EPSILON);
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
