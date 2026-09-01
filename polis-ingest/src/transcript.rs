//! Channel D — JSONL transcript tailing (PRD §4.4).
//!
//! > Used for: cold-start state rebuild, replay mode (§15 M2), and filling gaps
//! > where OTel dropped events.
//!
//! # A session is a forest, not a tree
//!
//! PRD §4.4 says "records chained by `parentUuid` — so a session is a tree".
//! Measured over 866 files: it is a **forest**. The main transcript plus one
//! independently-rooted tree per subagent — `parentUuid` is null on the first
//! record of all 643 subagent files and no `uuid` edge ever crosses between
//! files — interleaved with flat session sidecars. Only 4 of the 19 record types
//! are threaded at all; about **25% of lines have no `uuid`**. Cross-file
//! parenthood is carried by `agent-<id>.meta.json`, not by `parentUuid`
//! (ADR-0013).
//!
//! # Where the files are
//!
//! ```text
//! <munged-cwd>/<session-id>.jsonl                                     main
//! <munged-cwd>/<session-id>/subagents/agent-<agentId>.jsonl           direct spawn
//! <munged-cwd>/<session-id>/subagents/workflows/wf_<runId>/agent-*    workflow spawn
//! <munged-cwd>/<session-id>/subagents/workflows/wf_<runId>/journal.jsonl
//! ```
//!
//! 643 of 811 files (79%) are subagent transcripts. The depth is **variable**, so
//! glob for `agent-*.jsonl`; never assume a fixed depth. Watch the
//! `<session-id>/` sidecar *directory*, not just the file, because subagent files
//! appear at runtime.
//!
//! # Two traps
//!
//! * **The project key can be overridden by the CLI.** A session's directory is
//!   not always derivable from `cwd`, so Polis must *discover* directories under
//!   `~/.claude/projects` as well as compute them.
//! * **`toolUseResult` is absent on 65% of subagent tool results** (14 811 of
//!   22 697), including 625 `Edit` calls, and 100% present on main-thread
//!   results. Anything that reads paths, diff line counts or exit status out of
//!   the sidecar works perfectly in single-threaded testing and silently loses
//!   most of its data the moment subagents are involved (ADR-0004).

use std::path::{Path, PathBuf};

use polis_events::{SessionId, WorkerId};

use crate::bus::EventSink;

/// Tails one session's whole file set.
#[derive(Debug)]
pub struct TranscriptTailer {
    _private: (),
}

impl TranscriptTailer {
    /// Starts tailing a session: the main transcript plus its sidecar directory.
    pub fn start(session_dir: &Path, sink: EventSink) -> anyhow::Result<Self> {
        let _ = (session_dir, sink);
        todo!("PRD §4.4 — tail the main file and watch <session-id>/ for new agent files")
    }

    /// Replays a transcript offline as fast as it can be parsed (PRD §15 M2).
    ///
    /// The fastest iteration loop the project has: minutes per iteration, real
    /// data, no live infrastructure.
    pub fn replay(path: &Path, sink: EventSink) -> anyhow::Result<u64> {
        let _ = (path, sink);
        todo!("PRD §15 M2 — offline replay, ordered by (file, byte_offset)")
    }
}

/// Computes `~/.claude/projects/<project-key>` for a working directory
/// (PRD §4.4).
///
/// Confirmed byte-for-byte against the live CLI. Three details that produce a
/// wrong directory if guessed:
///
/// 1. The hash is over the **original** `cwd`, not the munged string.
/// 2. It is a JavaScript 32-bit `h = (h << 5) - h + c` over **UTF-16 code
///    units**, rendered base-36.
/// 3. `Math.abs` on `i32::MIN` yields 2 147 483 648 in JavaScript, so Rust must
///    widen to `i64` before taking the absolute value or it panics.
///
/// The key can also be overridden by the CLI, so this is a *hint*: always pair
/// it with directory discovery.
pub fn project_key(cwd: &str) -> String {
    let _ = cwd;
    todo!("docs/verified/jsonl-schema.md §1 — munge to 200 chars, then base36 of the i32 hash")
}

/// Enumerates the transcript files belonging to a session.
///
/// Globs for `agent-*.jsonl` at any depth under `subagents/`, because a workflow
/// agent sits two directories deeper than a directly spawned one.
pub fn session_files(session_dir: &Path) -> std::io::Result<Vec<TranscriptFile>> {
    let _ = session_dir;
    todo!("PRD §4.4 — glob, never assume depth")
}

/// One transcript file discovered on disk.
#[derive(Debug, Clone)]
pub struct TranscriptFile {
    /// Absolute path. May exceed `MAX_PATH` on Windows; `std::fs` copes, but
    /// nothing may hand it to an external process without a `\\?\` prefix.
    pub path: PathBuf,
    /// The session it belongs to. For a subagent file this is the **parent's**
    /// session id, which is exactly what the file itself records.
    pub session: SessionId,
    /// `Some` for a subagent transcript.
    pub agent: Option<WorkerId>,
}

/// Resolves a subagent to the `tool_use` that spawned it.
///
/// Ranked by reliability, because picking the easiest one to find is the trap:
///
/// 1. `agent-<id>.meta.json`'s `toolUseId` — **140 of 140**, but only if
///    `tool_use` ids are indexed across *every file* of the session. Per-file
///    indexing gives 117 of 140 and looks like the format is unreliable; the 23
///    misses are nested subagents whose spawning call lives in a parent
///    *subagent* file.
/// 2. `toolUseResult.agentId` in the parent — 117 of 140; misses nested agents.
/// 3. `promptId` — 616 of 641. A good "everything this turn produced" grouping
///    key and **not** a parent pointer.
///
/// Workflow subagents (503 of 643, 78%) have neither `toolUseId` nor
/// `parentAgentId`; they link structurally by the `wf_<runId>` directory matched
/// against the parent's `Workflow` `toolUseResult.runId`.
pub fn resolve_parent(meta_json: &Path, session_index: &ToolUseIndex) -> Option<WorkerId> {
    let _ = (meta_json, session_index);
    todo!("docs/verified/jsonl-schema.md §8 — index session-wide, then resolve")
}

/// A session-wide index of `tool_use` ids. Must span every file of the session.
#[derive(Debug, Default)]
pub struct ToolUseIndex {
    _private: (),
}
