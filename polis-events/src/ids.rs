//! Identity types (PRD §3 glossary, §5 world model).
//!
//! The single most expensive mistake available in this file is keying a thread
//! on `session_id` alone. Verified against 61 239 subagent records: **a subagent
//! transcript carries the *parent's* `sessionId`**, identical across every
//! subagent of that session (`docs/verified/jsonl-schema.md` §8). The hooks
//! channel agrees — `session_id` "stays the parent session's id inside a
//! subagent" (`docs/verified/hooks-schema.md` §2). So the thread key is
//! `(session_id, agent_id)`, with `agent_id` absent meaning the main agent.
//!
//! `attributionAgent` looks like a thread id and is not: it holds the agent
//! *type* (`workflow-subagent`, `Explore`, `Plan`, `general-purpose`) and never
//! equals an `agentId` in 36 952 comparisons. That is [`AgentType`], not
//! [`WorkerId`].

use std::fmt;

use serde::{Deserialize, Serialize};

/// Declares a string newtype with the boilerplate every id here shares.
macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident, $what:literal) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Wraps a raw ", $what, " as it arrived on the wire. No validation: ")]
            /// every channel is beta or undocumented and may widen its format.
            pub fn new(raw: impl Into<String>) -> Self {
                Self(raw.into())
            }

            #[doc = concat!("The raw ", $what, ".")]
            #[inline]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// True when the id is the empty string, which several channels use
            /// in place of omitting the field.
            #[inline]
            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({:?})"), self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }
    };
}

id_newtype!(
    /// One Claude Code process (PRD §3, "Session").
    ///
    /// Present on **every** record of every channel, including OTLP metric data
    /// points, which is why it and not `prompt.id` is the session key. It is
    /// *not* a resource attribute — the OTLP resource block carries exactly five
    /// keys and `session.id` is stamped per record instead
    /// (`docs/verified/otlp-receiver.md`).
    SessionId,
    "session id"
);

id_newtype!(
    /// A subagent (PRD §3, "Worker"); `agent_id` on hooks, `agentId` in
    /// transcripts, `agent_id` on the `claude_code.tool` span.
    ///
    /// **Presence is the discriminator.** Absent means main agent, never "parse
    /// failed" (`docs/verified/hooks-schema.md` §2.1). Format observed: `a` +
    /// 16 lowercase hex.
    WorkerId,
    "worker (subagent) id"
);

id_newtype!(
    /// The turn correlation key: `prompt.id` on OTel, `prompt_id` on hooks.
    ///
    /// A **turn** key, not a session key. Absent on all startup traffic
    /// (`plugin_loaded`, `permission_mode_changed`, the pre-prompt
    /// `api_request`) and never present on metrics. Requires Claude Code
    /// v2.1.196+. Must never be required by a parser.
    PromptId,
    "prompt id"
);

id_newtype!(
    /// `toolu_…` — the strongest cross-channel join key Polis has.
    ///
    /// Joins `tool_decision` ↔ `tool_result` ↔ the `claude_code.tool` span ↔
    /// hook payloads ↔ a transcript `tool_use` block. Resolve it **session-wide**
    /// across every transcript file, not per file: 23 of 140 subagents are
    /// nested and their spawning `tool_use` lives in a parent *subagent* file
    /// (`docs/verified/jsonl-schema.md` §8).
    ToolUseId,
    "tool use id"
);

id_newtype!(
    /// The *type* of an agent: `Explore`, `Plan`, `general-purpose`,
    /// `workflow-subagent`, a custom frontmatter name, or `my-plugin:reviewer`.
    ///
    /// Not an identity. A main agent launched with `claude --agent foo` also
    /// carries an agent type with no [`WorkerId`], so this must never be used to
    /// decide whether a record came from a worker.
    AgentType,
    "agent type"
);

/// How many identity hue slots the product has (PRD §11.4).
///
/// This is the length of `polis_render::live::THREAD_HUES`, a table of colours
/// that lives two crates away — and it is declared *here*, in the event model,
/// because the slot a thread gets is now decided by `polis_world::World` when
/// the thread is created, and `polis_world` cannot see `polis_render`. A
/// `const _: () = assert!(THREAD_HUES.len() == IDENTITY_SLOTS as usize)` beside
/// the table is what keeps the two from drifting; without it a thirteenth hue
/// would be authored and never handed out, silently.
///
/// Twelve is the *exclusivity ceiling*, not a collision rate: a world hands the
/// first twelve threads it ever sees twelve different slots, and the thirteenth
/// falls back to its bare [`ThreadId::hue_preference`], which is guaranteed to
/// collide. Twelve rather than twenty because the separation between hues is
/// what the operator actually reads and it is already at its floor —
/// `THREAD_HUES`'s own ΔE00 table measures the cloud fringe's worst pair at
/// 5.46, and twenty slots would take the rail swatch's worst pair from 9.80 to
/// 5.4. See that constant for the arithmetic.
pub const IDENTITY_SLOTS: u8 = 12;

/// FNV-1a's 32-bit offset basis, written out (ADR-0029).
const FNV_OFFSET: u32 = 2_166_136_261;
/// FNV-1a's 32-bit prime.
const FNV_PRIME: u32 = 16_777_619;

/// A thread: a main agent plus its worker subtree (PRD §3).
///
/// The unit the operator thinks in, and the key of `World::threads` in PRD §5.
/// A thread is identified by its session; workers within it are identified by
/// [`WorkerId`]. Constructing this from a subagent record's `sessionId` is
/// correct precisely *because* that field holds the parent's id.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThreadId(SessionId);

impl ThreadId {
    /// The thread that owns `session`.
    pub fn of_session(session: SessionId) -> Self {
        Self(session)
    }

    /// The session this thread is.
    #[inline]
    pub fn session(&self) -> &SessionId {
        &self.0
    }

    /// The raw session id string.
    #[inline]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// The hue slot this thread *prefers*, from its own identity and nothing
    /// else (PRD §11.4).
    ///
    /// # A preference, not the answer
    ///
    /// This used to be the whole rule, under the name `thread_slot`, and the
    /// operator rejected it: *"the same color is super bad, can you make sure
    /// the colors have to be different?"*. A hash over twelve slots collides —
    /// with nine threads on screen, about three of the thirty-six pairs — and
    /// the old doc accepted that on purpose. What actually decides a thread's
    /// hue now is `polis_world::World`, which hands out each slot to at most one
    /// thread and records the answer on `Thread::tint`; this function is only
    /// the first slot that assignment tries.
    ///
    /// # Why a hash is still the right first preference
    ///
    /// An index into the live thread list is free and wrong. The list is sorted
    /// and re-sorted as threads arrive, finish and are retired, so the colour of
    /// a thread the operator is watching would change when an unrelated thread
    /// started — and the one thing this channel is for is the operator learning
    /// *"the blue one is the refactor"* inside a minute. A palette that
    /// reshuffles destroys that faster than no palette at all, because it
    /// teaches something false. Seeding the assignment from the id keeps the
    /// common case — a world with a handful of threads — at exactly the colours
    /// it had before this rule existed, and keeps two unrelated worlds showing
    /// one session the same colour.
    ///
    /// FNV-1a over the id's bytes, written out rather than taken from
    /// [`std::hash::DefaultHasher`], whose algorithm is explicitly not stable
    /// across Rust releases (ADR-0029) — a toolchain bump must not repaint the
    /// city. Pinned by literal expected values in this module's tests, the same
    /// way [`crate::LogicalPath::layout_seed`] is.
    ///
    /// Not case-folded, unlike `layout_seed`. A session id is an opaque token
    /// from another program, not a path this repository owns two spellings of.
    #[must_use]
    pub fn hue_preference(&self) -> u8 {
        let mut h = FNV_OFFSET;
        for b in self.as_str().as_bytes() {
            h ^= u32::from(*b);
            h = h.wrapping_mul(FNV_PRIME);
        }
        #[allow(clippy::cast_possible_truncation)] // modulo IDENTITY_SLOTS = 12
        {
            (h % u32::from(IDENTITY_SLOTS)) as u8
        }
    }
}

impl fmt::Debug for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ThreadId({:?})", self.0.as_str())
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl From<SessionId> for ThreadId {
    fn from(s: SessionId) -> Self {
        Self(s)
    }
}

/// Which physical checkout a path came from (PRD §7.6).
///
/// Worktrees are a **separate dimension layered over one shared base map** — a
/// filter, a tint, or a stacking offset, never a separate city. So this is
/// carried alongside a [`crate::LogicalPath`], never folded into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorktreeId(pub u32);

impl WorktreeId {
    /// The repository's primary checkout — the one `git worktree list` names
    /// first and the one the city's layout is keyed to.
    pub const PRIMARY: Self = Self(0);

    /// True for the primary checkout.
    #[inline]
    pub fn is_primary(self) -> bool {
        self == Self::PRIMARY
    }
}

impl fmt::Display for WorktreeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_primary() {
            f.write_str("primary")
        } else {
            write!(f, "wt{}", self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_id_is_the_session_id() {
        let s = SessionId::new("68160373-dead-beef-0000-000000000001");
        let t = ThreadId::of_session(s.clone());
        assert_eq!(t.session(), &s);
        assert_eq!(t.to_string(), s.to_string());
    }

    #[test]
    fn ids_round_trip_through_serde_transparently() {
        let w = WorkerId::new("a96cf8a57447af436");
        let json = serde_json::to_string(&w).unwrap();
        // `transparent` means no wrapper object: this is what makes the ids
        // droppable straight into a payload struct.
        assert_eq!(json, "\"a96cf8a57447af436\"");
        assert_eq!(serde_json::from_str::<WorkerId>(&json).unwrap(), w);
    }

    #[test]
    fn empty_ids_are_representable_because_channels_send_them() {
        assert!(SessionId::new("").is_empty());
        assert!(!SessionId::new("x").is_empty());
    }

    /// The hue preference is pinned by literal values (ADR-0029): if
    /// `DefaultHasher` crept in, or the constants were retyped, every city on
    /// every machine would repaint itself on a toolchain bump and nothing else
    /// would notice. Moved here from `polis_render::live::thread_slot` when the
    /// hash became the world's input rather than the renderer's output; the
    /// literals are unchanged, so no existing world changes colour.
    #[test]
    fn the_identity_hash_is_written_out_and_pinned() {
        let pref = |s: &str| ThreadId::of_session(SessionId::new(s)).hue_preference();
        #[allow(clippy::cast_possible_truncation)] // modulo IDENTITY_SLOTS = 12
        let empty = (FNV_OFFSET % u32::from(IDENTITY_SLOTS)) as u8;
        assert_eq!(pref(""), empty);
        assert_eq!(pref("a"), 4);
        // The collision the operator reported, reproduced from two ids a real
        // `World` can hold at once: both want slot 4 and only one may have it.
        assert_eq!(pref("polis"), 4);
        assert_eq!(pref("4f3a1c22-0e5b-4b8a-9d21-6c7e5f0a1b2c"), 1);
        assert!(pref("9b2e77d0-1111-4aaa-8bbb-ccccddddeeee") < IDENTITY_SLOTS);
    }

    /// The preference is a pure function of the id and of nothing else — no
    /// list to be in, no clock, no other thread. That is what makes it a sound
    /// *seed* for an assignment that does depend on other threads.
    #[test]
    fn a_threads_hue_preference_depends_on_no_other_thread() {
        let ids = [
            "4f3a1c22-0e5b-4b8a-9d21-6c7e5f0a1b2c",
            "9b2e77d0-1111-4aaa-8bbb-ccccddddeeee",
            "0000aaaa-2222-4ccc-8ddd-eeeeffff0000",
        ];
        let pref = |s: &str| ThreadId::of_session(SessionId::new(s)).hue_preference();
        let first: Vec<u8> = ids.iter().map(|i| pref(i)).collect();
        for perm in [[2usize, 0, 1], [1, 2, 0], [0, 2, 1]] {
            for k in perm {
                assert_eq!(pref(ids[k]), first[k], "the preference moved");
            }
        }
    }

    #[test]
    fn primary_worktree_is_zero() {
        assert!(WorktreeId::PRIMARY.is_primary());
        assert_eq!(WorktreeId::PRIMARY.to_string(), "primary");
        assert_eq!(WorktreeId(3).to_string(), "wt3");
        assert!(!WorktreeId(3).is_primary());
    }
}
