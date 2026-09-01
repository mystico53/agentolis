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

    #[test]
    fn primary_worktree_is_zero() {
        assert!(WorktreeId::PRIMARY.is_primary());
        assert_eq!(WorktreeId::PRIMARY.to_string(), "primary");
        assert_eq!(WorktreeId(3).to_string(), "wt3");
        assert!(!WorktreeId(3).is_primary());
    }
}
