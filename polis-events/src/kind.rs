//! The hook wire tag table (PRD §4.2 step 2).
//!
//! These `u32` values travel in the low 31 bits of the wire header's tag field.
//! **They must match `polis-hook/src/main.rs::event_tag` exactly.** `polis-hook`
//! cannot depend on this crate — PRD §14 requires its dependency tree to be
//! std-only — so the table is duplicated there and the two copies are pinned
//! together by `tests::hook_binary_table_matches`, which parses the hook's
//! source with `include_str!` and diffs it. That test is the only thing standing
//! between a rename here and silent misrouting of every hook event on the
//! machine.
//!
//! # `WorktreeCreate` is absent on purpose
//!
//! `docs/verified/hooks-schema.md` §9.1: a `WorktreeCreate` handler **replaces**
//! git's worktree creation and must print the path of the worktree it created.
//! `polis-hook` prints nothing and exits 0, so registering it would suppress
//! `git worktree` and then fail creation for want of a path — breaking every
//! `claude --worktree` session, every `isolation: "worktree"` subagent, and
//! every background session on the machine. It has no tag and never will.
//! Worktree births are observed through `SessionStart` + `cwd`, [`CwdChanged`],
//! or the filesystem watcher instead.
//!
//! [`CwdChanged`]: EventKind::CwdChanged

use std::fmt;

use serde::{Deserialize, Serialize};

/// A hook event kind, as tagged in the wire header (PRD §4.2).
///
/// The tag is a **routing hint only**. `hook_event_name` inside the JSON payload
/// is authoritative; the tag exists so `polis-ingest` can shard a datagram onto
/// the right handler without parsing JSON on the receive thread.
/// # Adding a variant is not a breaking change downstream
///
/// `#[non_exhaustive]`: ADR-0044 already promises this table is **append-only**,
/// because the discriminants are the wire tags `polis-hook` writes. The attribute
/// makes the compiler enforce the other half of that promise — an exhaustive
/// `match` in one of the seven downstream crates would otherwise turn "Claude
/// Code added an event" into a workspace-wide compile break. Matching inside
/// `polis-events` is unaffected, which is why the tables below still compile
/// without a wildcard arm (ADR-0048).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(u32)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub enum EventKind {
    /// Tag 0. An unrecognised `--event` name, or none at all. Still delivered:
    /// the daemon re-derives the true kind from the payload.
    Unknown = 0,
    /// A session begins or resumes. Cannot block. The hook must write nothing to
    /// stdout — plain-text stdout here is injected into Claude's context.
    SessionStart = 1,
    /// A session terminates. Cannot block. **1.5 s budget shared across all
    /// `SessionEnd` hooks.**
    SessionEnd = 2,
    /// Before a tool call executes. Registered **narrowed** to
    /// `^(Edit|Write|NotebookEdit)$` — the §11.3 contention claim needs it, and
    /// an unmatched registration is the ~200 calls/sec firehose PRD §4.2 exists
    /// to avoid. There is no `MultiEdit` tool.
    PreToolUse = 3,
    /// After a tool call fails. Cannot block.
    PostToolUseFailure = 4,
    /// A tool call needs a permission decision. Feeds attention state (a),
    /// PRD §11.2. Exit 2 is *not* honoured here; only a `decision` object can
    /// allow or deny, so a silent exit-0 hook is safe.
    PermissionRequest = 5,
    /// A subagent is spawned (PRD §3, "Worker").
    SubagentStart = 6,
    /// A subagent finishes. Carries `agent_transcript_path`, which is how the
    /// tailer discovers subagent transcripts (PRD §4.4 as corrected, ADR-0013).
    /// **Exit 2 would prevent the subagent from stopping.**
    SubagentStop = 7,
    /// A task is created via `TaskCreate`. **Exit 2 rolls back creation.**
    TaskCreated = 8,
    /// A task is marked completed. **Exit 2 prevents completion.**
    TaskCompleted = 9,
    /// Claude finishes responding — a main thread going idle, which is
    /// attention state (b) in PRD §11.2. **Exit 2 prevents Claude stopping.**
    Stop = 10,
    /// The turn ended due to an API error. Output and exit code are ignored
    /// entirely; the safest event on the list.
    StopFailure = 11,
    /// An agent-team teammate is about to go idle. Feeds attention state (a).
    /// **Exit 2 prevents the teammate going idle.**
    TeammateIdle = 12,
    /// A notification. Registered narrowed to
    /// `permission_prompt|idle_prompt|agent_needs_input`; these fire even with
    /// desktop notifications disabled. Cannot block, exit code ignored.
    Notification = 13,
    /// An MCP server requests user input during a tool call. Feeds attention
    /// state (a). **Exit 2 denies the elicitation.**
    Elicitation = 14,
    /// The user responded to an MCP elicitation. **Exit 2 turns the response
    /// into `decline`.**
    ElicitationResult = 15,
    /// A worktree is being removed. Cannot block; failures logged in debug mode
    /// only. Safe. (Its `WorktreeCreate` sibling is deliberately unregistered.)
    WorktreeRemove = 16,
    /// The working directory changed — `cd`, or Claude entering a worktree.
    /// `old_cwd`/`new_cwd` support the PRD §7.6 worktree-prefix stripping.
    CwdChanged = 17,
    /// Before context compaction. **Exit 2 blocks compaction.**
    PreCompact = 18,
    /// After context compaction. Cannot block. Safe.
    PostCompact = 19,
}

impl EventKind {
    /// The largest tag a well-formed frame may carry.
    ///
    /// `polis-ingest` rejects a datagram whose kind exceeds this, which together
    /// with the `len + 8 == datagram_len` check replaces a magic number
    /// (`docs/verified/hook-ipc.md`).
    pub const MAX_TAG: u32 = Self::PostCompact as u32;

    /// Every kind, in tag order. `ALL[n].as_tag() == n` holds and is asserted.
    pub const ALL: [Self; 20] = [
        Self::Unknown,
        Self::SessionStart,
        Self::SessionEnd,
        Self::PreToolUse,
        Self::PostToolUseFailure,
        Self::PermissionRequest,
        Self::SubagentStart,
        Self::SubagentStop,
        Self::TaskCreated,
        Self::TaskCompleted,
        Self::Stop,
        Self::StopFailure,
        Self::TeammateIdle,
        Self::Notification,
        Self::Elicitation,
        Self::ElicitationResult,
        Self::WorktreeRemove,
        Self::CwdChanged,
        Self::PreCompact,
        Self::PostCompact,
    ];

    /// The wire tag.
    #[inline]
    pub fn as_tag(self) -> u32 {
        self as u32
    }

    /// Decodes a wire tag. Out-of-range tags become [`EventKind::Unknown`]
    /// rather than an error: a future Claude Code release adding an event that a
    /// future `polis-hook` tags is a *drift* condition, not a fatal one.
    pub fn from_tag(tag: u32) -> Self {
        Self::ALL
            .get(tag as usize)
            .copied()
            .unwrap_or(Self::Unknown)
    }

    /// The `--event` name `polis install-hooks` writes into `settings.json`, and
    /// the value of `hook_event_name` in the payload.
    pub fn name(self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::PermissionRequest => "PermissionRequest",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::TaskCreated => "TaskCreated",
            Self::TaskCompleted => "TaskCompleted",
            Self::Stop => "Stop",
            Self::StopFailure => "StopFailure",
            Self::TeammateIdle => "TeammateIdle",
            Self::Notification => "Notification",
            Self::Elicitation => "Elicitation",
            Self::ElicitationResult => "ElicitationResult",
            Self::WorktreeRemove => "WorktreeRemove",
            Self::CwdChanged => "CwdChanged",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
        }
    }

    /// Resolves `hook_event_name` from the payload — the authoritative source.
    ///
    /// Returns [`EventKind::Unknown`] for any name Polis does not register,
    /// including `WorktreeCreate`, which must never appear.
    pub fn from_name(name: &str) -> Self {
        Self::ALL
            .iter()
            .copied()
            .skip(1)
            .find(|k| k.name() == name)
            .unwrap_or(Self::Unknown)
    }

    /// True for the events where a non-zero exit, or stdout, changes what the
    /// agent does.
    ///
    /// `polis-hook` never exits non-zero and never writes stdout, so this is
    /// documentation and a test hook rather than a runtime branch — but it is
    /// the list a reviewer should check any future hook change against
    /// (`docs/verified/hooks-schema.md` §4.4).
    pub fn is_blocking_capable(self) -> bool {
        matches!(
            self,
            Self::PreToolUse
                | Self::SubagentStop
                | Self::TaskCreated
                | Self::TaskCompleted
                | Self::Stop
                | Self::TeammateIdle
                | Self::Elicitation
                | Self::ElicitationResult
                | Self::PreCompact
        )
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_is_indexed_by_tag() {
        for (i, k) in EventKind::ALL.iter().enumerate() {
            assert_eq!(k.as_tag() as usize, i, "ALL is out of tag order at {i}");
        }
        assert_eq!(EventKind::MAX_TAG, 19);
    }

    #[test]
    fn names_round_trip_and_are_unique() {
        let mut names: Vec<&str> = EventKind::ALL.iter().map(|k| k.name()).collect();
        let n = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), n, "duplicate event name");

        for k in EventKind::ALL.iter().copied().skip(1) {
            assert_eq!(EventKind::from_name(k.name()), k);
            assert_eq!(EventKind::from_tag(k.as_tag()), k);
        }
    }

    #[test]
    fn worktree_create_has_no_tag() {
        // docs/verified/hooks-schema.md §9.1. Registering it breaks every
        // worktree on the machine.
        assert_eq!(EventKind::from_name("WorktreeCreate"), EventKind::Unknown);
        assert!(!EventKind::ALL.iter().any(|k| k.name() == "WorktreeCreate"));
    }

    #[test]
    fn unknown_tags_degrade_instead_of_failing() {
        for tag in [20_u32, 99, 1_000_000, u32::MAX, u32::MAX - 1] {
            assert_eq!(EventKind::from_tag(tag), EventKind::Unknown);
        }
        assert_eq!(EventKind::from_name(""), EventKind::Unknown);
        assert_eq!(EventKind::from_name("Unknown"), EventKind::Unknown);
        assert_eq!(EventKind::from_name("preToolUse"), EventKind::Unknown);
    }

    /// The reason this crate and `polis-hook` cannot drift apart.
    ///
    /// `polis-hook` has no dependencies (PRD §14) so it cannot import
    /// [`EventKind`]. `include_str!` is not a dependency — it is a compile-time
    /// file read — so this test can still hold the two tables together.
    #[test]
    fn hook_binary_table_matches() {
        const HOOK_SRC: &str = include_str!("../../polis-hook/src/main.rs");

        // Isolate `fn event_tag`'s body, so a matching string elsewhere in the
        // file (a doc comment, a test) cannot make this pass by accident.
        let body = HOOK_SRC
            .split_once("fn event_tag(name: &str) -> u32 {")
            .expect("polis-hook lost its event_tag function")
            .1
            .split_once("\n}")
            .expect("event_tag has no closing brace")
            .0;

        let mut arms: Vec<(String, u32)> = Vec::new();
        for line in body.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix('"') else {
                continue; // the `_ => 0` fallback and blank lines
            };
            let (name, rest) = rest.split_once('"').expect("unterminated arm literal");
            let tag: u32 = rest
                .trim_start_matches(" => ")
                .trim_end_matches(',')
                .parse()
                .expect("non-numeric tag in polis-hook");
            arms.push((name.to_owned(), tag));
        }

        assert_eq!(
            arms.len(),
            EventKind::ALL.len() - 1,
            "polis-hook has {} arms, EventKind has {} named kinds",
            arms.len(),
            EventKind::ALL.len() - 1
        );
        for (name, tag) in &arms {
            let k = EventKind::from_name(name);
            assert_ne!(
                k,
                EventKind::Unknown,
                "polis-hook tags unknown event {name}"
            );
            assert_eq!(k.as_tag(), *tag, "tag mismatch for {name}");
        }
        assert!(
            !arms.iter().any(|(n, _)| n == "WorktreeCreate"),
            "polis-hook must never tag WorktreeCreate"
        );
    }

    #[test]
    fn hook_binary_agrees_on_the_wire_constants() {
        const HOOK_SRC: &str = include_str!("../../polis-hook/src/main.rs");
        for needle in [
            "const HEADER_LEN: usize = 8;",
            "const MAX_PAYLOAD: usize = 60 * 1024;",
            "const TAG_TRUNCATED: u32 = 0x8000_0000;",
            "const DEFAULT_PORT: u16 = 45177;",
            "const ENDPOINT_ENV: &str = \"POLIS_HOOK_ENDPOINT\";",
        ] {
            assert!(HOOK_SRC.contains(needle), "polis-hook changed: {needle}");
        }
    }
}
