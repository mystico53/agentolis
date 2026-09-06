//! Channel D — JSONL transcript tailing (PRD §4.4).
//!
//! > Used for: cold-start state rebuild, replay mode (§15 M2), and filling gaps
//! > where OTel dropped events.
//!
//! This module owns the whole of Channel D: the defensive record model, the
//! per-line parser PRD §16 fuzzes, discovery of the project directories, the
//! live tailer, and the offline reader `polis replay` is built on.
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
//! [`session_files`] globs for `agent-*.jsonl`; never assume a fixed depth. The
//! tailer watches the `<session-id>/` sidecar *directory*, not just the file,
//! because subagent files appear at runtime.
//!
//! # Two traps
//!
//! * **The project key can be overridden by the CLI.** A session's directory is
//!   not always derivable from `cwd`, so Polis *discovers* directories under
//!   `~/.claude/projects` ([`index_projects`]) as well as computing them
//!   ([`project_key`]) — ADR-0033.
//! * **`toolUseResult` is absent on 65% of subagent tool results** (14 811 of
//!   22 697), including 625 `Edit` calls, and 100% present on main-thread
//!   results. Anything that reads paths, diff line counts or exit status out of
//!   the sidecar works perfectly in single-threaded testing and silently loses
//!   most of its data the moment subagents are involved (ADR-0004).
//!
//! # The single hard requirement
//!
//! **A parse failure on one line must never abort the file.** [`parse_line`] is
//! total: every `&str` produces a [`LineOutcome`], never an `Err` and never a
//! panic. Bad lines advance the cursor and increment a counter on [`ParseStats`],
//! which is the schema-drift signal PRD §16 asks the status bar to show.
//! [`fuzz_transcript_bytes`] is the entry point a `cargo-fuzz` target calls.
//!
//! # Ordering
//!
//! Record order is `(file, byte_offset)` — **never** the record's own
//! `timestamp`. 162 of 808 files contain a backwards step and one observed jump
//! was 60 seconds (ADR-0014). The offline reader monotonises timestamps in byte
//! order before writing them into [`RecordedEvent::wall`], so that a recording's
//! wall clock and its `monotonic_offset_ms` sort identically as ADR-0049
//! requires. The verbatim `timestamp` survives inside the record payload.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Map, Value};

use polis_events::{
    AgentType, Channel, ControlEvent, Event, EventMeta, PathMapper, Payload, PromptId,
    RecordedEvent, RecordingHeader, ReplayClock, SessionId, ToolUseId, TranscriptEvent,
    TranscriptRecordKind, TranscriptSource, WallTime, WorkerId, RECORDING_FORMAT,
};

use crate::bus::EventSink;
use crate::{IngestSource, SourceHealth};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// How long the tailer thread sleeps between polls when the watcher is quiet.
///
/// The watcher is only a wake-up hint: `notify` is untested against a
/// `>MAX_PATH` project directory (ADR-0034), so the loop must make progress
/// without it.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Polls between rescans for files that appeared at runtime.
///
/// Subagent transcripts are created mid-session, and a watcher that failed to
/// install would otherwise never notice them.
pub const RESCAN_EVERY: u32 = 8;

/// Most bytes one [`FileTail::poll`] will read from one file.
///
/// A cold start on a 100 MB transcript must not stall the tailer thread for a
/// second; the remainder is picked up on the next poll.
pub const MAX_POLL_BYTES: u64 = 8 * 1024 * 1024;

/// Longest string leaf kept verbatim inside an emitted record.
///
/// Transcript records carry whole files, whole prompts and whole `stdout`
/// buffers. The bus holds 65 536 events (PRD §4.5), so an unbounded record turns
/// a busy session into gigabytes of resident queue, and `polis replay`'s
/// recording into a copy of the source tree. Longer strings are replaced by
/// their prefix plus `…[N chars, M lines elided]`, which preserves the two
/// numbers §7.3 and §7.4 actually want (line deltas and output size) — the same
/// treatment the checked-in fixtures got. Use
/// [`elide_long_strings`] with [`usize::MAX`] to opt out.
pub const MAX_RECORD_STRING: usize = 2048;

/// The 200-character cap the CLI applies to a project directory name.
const PROJECT_KEY_LIMIT: usize = 200;

/// Bytes of a transcript read when probing it for its `cwd`.
///
/// Public because [`index_projects`] and [`project_dir_cwd`] both promise it as
/// a bound in their own documentation, and a bound a caller cannot name is a
/// number in a sentence rather than a contract.
pub const CWD_PROBE_BYTES: u64 = 256 * 1024;

/// Deepest directory nesting [`session_files`] will walk under `subagents/`.
///
/// The observed maximum is 2 (`workflows/wf_<runId>`); the cap exists so a
/// symlink loop cannot turn discovery into an infinite walk.
const MAX_SUBAGENT_DEPTH: usize = 8;

// ---------------------------------------------------------------------------
// The record model (PRD §4.4: known fields + a `Value` catch-all)
// ---------------------------------------------------------------------------

/// The envelope carried by the four **threaded** record types.
///
/// Every field is `Option` even where the corpus says 100%: this is undocumented
/// internals and absence is routine. `#[serde(rename_all = "camelCase")]` is
/// load-bearing — without it `toolUseResult`, `promptId`, `requestId`, `agentId`
/// and `isMeta` all deserialize to `None` and **serde reports no error at all**,
/// because every field is optional and there is a catch-all (ADR-0035).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    /// Unique within the file; 0 duplicates in 124 262 records.
    pub uuid: Option<String>,
    /// Explicitly `null` on a tree root. The key exists; the value may be null.
    pub parent_uuid: Option<String>,
    /// After a `system`/`compact_boundary` record the pre-compaction thread is
    /// reachable only through this. Following `parentUuid` alone renders a
    /// compacted session as two unrelated trees.
    pub logical_parent_uuid: Option<String>,
    /// `true` exactly when the record lives in a `subagents/` file.
    pub is_sidechain: Option<bool>,
    /// ISO-8601 UTC, millisecond precision. **Display only** — see the module
    /// docs on ordering.
    pub timestamp: Option<String>,
    /// The *file's* session id, including inside a subagent file, where it is
    /// the parent's id rather than a new one.
    pub session_id: Option<String>,
    /// The `snake_case` `session_id` key, which is **not** a duplicate of
    /// [`session_id`](Self::session_id): where the two differ, this one names the
    /// ancestor session the current one was resumed or forked from (equal on
    /// 22 180 records, different on 38 153). Modelled separately on purpose —
    /// `rename_all = "camelCase"` maps them exactly backwards.
    #[serde(rename = "session_id")]
    pub ancestor_session_id: Option<String>,
    /// Absolute working directory. Backslashed on Windows.
    pub cwd: Option<String>,
    /// Can be the literal `"HEAD"` (detached), not always a branch name.
    pub git_branch: Option<String>,
    /// The Claude Code release that wrote the record. The drift signal.
    pub version: Option<String>,
    /// `"external"` in every record observed.
    pub user_type: Option<String>,
    /// `"cli"` or `"sdk-cli"`.
    pub entrypoint: Option<String>,
    /// Present exactly when the record is in a subagent file, and always equal
    /// to the agent id in the filename.
    pub agent_id: Option<String>,
    /// Human nickname for the session, e.g. `graceful-leaping-steele`.
    pub slug: Option<String>,
}

/// One `user` / `assistant` / `attachment` / `system` record.
///
/// A single struct for all four rather than four near-identical ones: the four
/// share the whole envelope and differ only in which optional bodies are
/// populated, and the variant of [`Record`] already says which type it is. One
/// struct means one place to get `rename_all` right (ADR-0035).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadedRecord {
    /// The shared envelope.
    #[serde(flatten)]
    pub envelope: Envelope,
    /// `user` and `assistant` bodies.
    pub message: Option<Message>,
    /// `attachment` bodies. 25 payload shapes, modelled as a key map.
    pub attachment: Option<Attachment>,
    /// `system` discriminator: `turn_duration`, `compact_boundary`, …
    pub subtype: Option<String>,
    /// The per-tool structured sidecar. **Never load-bearing**: absent on 65% of
    /// subagent tool results (ADR-0004). Kept as a `Value` because there are 62
    /// tools, the shape is not tagged, and it is a bare string on the error path.
    pub tool_use_result: Option<Value>,
    /// Groups a whole turn. A grouping key, not a parent pointer.
    pub prompt_id: Option<String>,
    /// The API request that produced an assistant record.
    pub request_id: Option<String>,
    /// Points at the assistant record that issued the `tool_use`. Equal to
    /// `parentUuid` in 37 912 of 37 912 records — a free integrity check.
    /// Needs an explicit rename: camelCase of `..._uuid` is `...Uuid`, not
    /// `...UUID`.
    #[serde(rename = "sourceToolAssistantUUID")]
    pub source_tool_assistant_uuid: Option<String>,
    /// The agent **type** (`Explore`, `workflow-subagent`, …), never an id. It
    /// never equals `agentId` in 36 952 comparisons (§8.5).
    pub attribution_agent: Option<String>,
    /// The skill that produced the turn, when one did.
    pub attribution_skill: Option<String>,
    /// Injected rather than typed by the operator.
    pub is_meta: Option<bool>,
    /// `system`/`turn_duration` only.
    pub duration_ms: Option<u64>,
    /// `system`/`turn_duration` only.
    pub message_count: Option<u64>,
    /// `system`/`compact_boundary` only.
    pub compact_metadata: Option<Value>,
    /// `"xhigh"` | `"high"` | `"max"` on assistant records.
    pub effort: Option<String>,
    /// Absent means false.
    pub is_api_error_message: Option<bool>,
    /// `user-rejected` | `permission-rule` | `automode-blocked` | …
    pub tool_denial_kind: Option<String>,
    /// `{kind}` ∈ `human` | `task-notification` | `coordinator`.
    pub origin: Option<Value>,
    /// **The PRD §4.4 catch-all.** Fields appear between CLI releases
    /// (`queueSkipAttachments` and `sdk-cli` both arrived inside the observed
    /// corpus); keeping them makes drift observable instead of silent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A `message` body. Its keys are already `snake_case` on the wire
/// (`stop_reason`, `tool_use_id`), so this struct deliberately carries **no**
/// `rename_all`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Message {
    /// `"user"` or `"assistant"`.
    pub role: Option<String>,
    /// Assistant only, e.g. `claude-opus-5` or `<synthetic>`.
    pub model: Option<String>,
    /// `msg_…`.
    pub id: Option<String>,
    /// Blocks, or a bare string on 4% of user records.
    pub content: Option<Content>,
    /// `tool_use` | `end_turn` | `stop_sequence` | null.
    pub stop_reason: Option<String>,
    /// Token accounting; version-dependent, so it stays a `Value`.
    pub usage: Option<Value>,
    /// Catch-all: `context_management` and `container` appear on <0.1%.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `message.content`, which is an array on assistant records and **either** an
/// array or a bare string on user records (1 616 of 39 686).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Content {
    /// A plain human turn.
    Text(String),
    /// The normal case.
    Blocks(Vec<Block>),
    /// Anything else this release invented. Never an error.
    Other(Value),
}

/// One content block: `tool_use`, `tool_result`, `text`, `thinking` or `image`.
///
/// No catch-all here on purpose — the unmodelled keys survive verbatim in the
/// raw record carried by [`TranscriptEvent::record`], and nesting a
/// `#[serde(flatten)]` inside an untagged enum buys nothing for it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Block {
    /// The block type.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// `toolu_…` on a `tool_use` block.
    pub id: Option<String>,
    /// The tool's name on a `tool_use` block. Note the subagent-spawning tool is
    /// `Agent`; there is no `Task` tool (§14.7).
    pub name: Option<String>,
    /// The tool's arguments. **Never `unwrap` a key**: `Read.file_path` is
    /// absent on 9 records, replaced by `__unparsedToolInput`.
    pub input: Option<Value>,
    /// `text` blocks.
    pub text: Option<String>,
    /// `thinking` blocks.
    pub thinking: Option<String>,
    /// Joins a `tool_result` back to its `tool_use`. Resolve it **session-wide**
    /// across every file, never per file (§8.3).
    pub tool_use_id: Option<String>,
    /// A string on 35 460 blocks, an array of blocks on 2 452.
    pub content: Option<Value>,
    /// Present on only 49.9% of `tool_result` blocks; **absent means not an
    /// error**, there is no third state.
    pub is_error: Option<bool>,
}

/// An `attachment` body: a `type` plus a per-type key set (25 seen, treat as
/// open).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Attachment {
    /// `total_tokens_reminder`, `file`, `nested_memory`, `hook_success`, …
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Everything else. `file` and `nested_memory` carry `filename`/`path` and
    /// are the only transcript record of a file the operator referenced with
    /// `@`, which produces no `Read` tool call.
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

/// A flat session sidecar: `{type, sessionId, 1-3 fields}`, no envelope, no
/// `uuid`, no position in any tree.
///
/// Fifteen of the 19 record types are these, and they are appended interleaved
/// with real records — about 25% of all lines. One permissive struct covers all
/// of them; [`Record::Sidecar`] carries which one it was.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarRecord {
    /// Present on every sidecar except the two `journal.jsonl` types.
    pub session_id: Option<String>,
    /// `mode`.
    pub mode: Option<String>,
    /// `permission-mode`.
    pub permission_mode: Option<String>,
    /// `ai-title`.
    pub ai_title: Option<String>,
    /// `custom-title`.
    pub custom_title: Option<String>,
    /// `agent-name`.
    pub agent_name: Option<String>,
    /// `atis-latch`.
    pub atis: Option<Value>,
    /// `last-prompt`: names the live leaf of the branching tree, which is what
    /// tells a renderer which chain to draw.
    pub leaf_uuid: Option<String>,
    /// `last-prompt`.
    pub last_prompt: Option<Value>,
    /// `queue-operation`: `enqueue` | `dequeue` | `remove` | `popAll`.
    pub operation: Option<String>,
    /// On `queue-operation` and `frame-link`.
    pub timestamp: Option<String>,
    /// On several sidecars.
    pub content: Option<Value>,
    /// `bridge-session`.
    pub bridge_session_id: Option<String>,
    /// `bridge-session`.
    pub last_sequence_num: Option<i64>,
    /// `frame-link`.
    pub frame_url: Option<String>,
    /// `frame-link`.
    pub path: Option<String>,
    /// `frame-link`.
    pub title: Option<String>,
    /// `cost-state`. Needs an explicit rename — camelCase of `total_cost_usd` is
    /// `totalCostUsd`, and the wire key is `totalCostUSD`.
    #[serde(rename = "totalCostUSD")]
    pub total_cost_usd: Option<f64>,
    /// `cost-state`: a free, exact, session-cumulative diff-line counter —
    /// PRD §7.3's height signal without reconstructing it from patches. Written
    /// at session end, so it is a reconciliation input, not a live one.
    pub total_lines_added: Option<i64>,
    /// `cost-state`.
    pub total_lines_removed: Option<i64>,
    /// `file-history-snapshot` / `file-history-delta`.
    pub message_id: Option<String>,
    /// `file-history-delta`.
    pub snapshot_message_id: Option<String>,
    /// `file-history-delta`: a second, independent list of files an agent
    /// touched, maintained by Claude Code's own undo bookkeeping.
    pub tracking_path: Option<String>,
    /// `file-history-snapshot`.
    pub snapshot: Option<Value>,
    /// `file-history-delta`.
    pub backup: Option<Value>,
    /// The PRD §4.4 catch-all.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A `journal.jsonl` record: `started` or `result`. No envelope, no timestamp.
///
/// Every one of 504 `started` records has a sibling `agent-<id>.jsonl` and every
/// sibling has a `started` — a perfect bijection — which makes the journal the
/// cheapest liveness signal a workflow fleet has.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalRecord {
    /// `v2:<64-hex>`.
    pub key: Option<String>,
    /// The agent this line is about.
    pub agent_id: Option<String>,
    /// On `result` only: an object (the agent's `StructuredOutput`) or a bare
    /// string (its final message).
    pub result: Option<Value>,
    /// The PRD §4.4 catch-all.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One parsed transcript line.
///
/// `#[non_exhaustive]` for the same reason
/// [`polis_events::TranscriptRecordKind`] is: the record-type count went from
/// "a handful" to 19 inside one observed release window (ADR-0048).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Record {
    /// A human turn **or** a tool result.
    User(Box<ThreadedRecord>),
    /// One model response.
    Assistant(Box<ThreadedRecord>),
    /// Injected context.
    Attachment(Box<ThreadedRecord>),
    /// Turn duration, compaction boundaries, local commands.
    System(Box<ThreadedRecord>),
    /// One of the 15 flat session sidecars.
    Sidecar {
        /// Which sidecar.
        kind: TranscriptRecordKind,
        /// Its fields.
        body: Box<SidecarRecord>,
    },
    /// A `journal.jsonl` `started` / `result` line.
    Journal {
        /// Which of the two.
        kind: TranscriptRecordKind,
        /// Its fields.
        body: Box<JournalRecord>,
    },
    /// A record type this Claude Code release added. Counted as drift, never
    /// fatal — 15 of the 19 known types are cosmetic and new ones are cheap to
    /// ignore.
    Unknown {
        /// The `type` string, when the line even had one.
        raw_type: Option<String>,
    },
}

impl Record {
    /// The record's kind, for [`TranscriptEvent::kind`].
    pub fn kind(&self) -> TranscriptRecordKind {
        match self {
            Self::User(_) => TranscriptRecordKind::User,
            Self::Assistant(_) => TranscriptRecordKind::Assistant,
            Self::Attachment(_) => TranscriptRecordKind::Attachment,
            Self::System(_) => TranscriptRecordKind::System,
            Self::Sidecar { kind, .. } | Self::Journal { kind, .. } => kind.clone(),
            Self::Unknown { .. } => TranscriptRecordKind::Unknown,
        }
    }

    /// The envelope, for the four threaded types. `None` for the 15 sidecars —
    /// which is the point: they have no `uuid` and no position in any tree.
    pub fn envelope(&self) -> Option<&Envelope> {
        match self {
            Self::User(r) | Self::Assistant(r) | Self::Attachment(r) | Self::System(r) => {
                Some(&r.envelope)
            }
            _ => None,
        }
    }

    /// The threaded body, when this is one of the four threaded types.
    pub fn threaded(&self) -> Option<&ThreadedRecord> {
        match self {
            Self::User(r) | Self::Assistant(r) | Self::Attachment(r) | Self::System(r) => Some(r),
            _ => None,
        }
    }

    /// The session this record belongs to, from whichever field carries it.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Sidecar { body, .. } => body.session_id.as_deref(),
            Self::Journal { .. } | Self::Unknown { .. } => None,
            _ => self.envelope().and_then(|e| e.session_id.as_deref()),
        }
    }

    /// The subagent that wrote this record, if any. Absence means main agent,
    /// never "parse failed".
    pub fn agent_id(&self) -> Option<&str> {
        match self {
            Self::Journal { body, .. } => body.agent_id.as_deref(),
            _ => self.envelope().and_then(|e| e.agent_id.as_deref()),
        }
    }

    /// The turn key, when the record carries one.
    pub fn prompt_id(&self) -> Option<&str> {
        self.threaded().and_then(|r| r.prompt_id.as_deref())
    }

    /// The CLI version that wrote the record — the leading indicator of drift.
    pub fn version(&self) -> Option<&str> {
        self.envelope().and_then(|e| e.version.as_deref())
    }

    /// The record's own timestamp. **Display only.**
    pub fn timestamp(&self) -> Option<&str> {
        match self {
            Self::Sidecar { body, .. } => body.timestamp.as_deref(),
            _ => self.envelope().and_then(|e| e.timestamp.as_deref()),
        }
    }

    /// The content blocks, when the record has any.
    pub fn blocks(&self) -> &[Block] {
        match self
            .threaded()
            .and_then(|r| r.message.as_ref())
            .and_then(|m| m.content.as_ref())
        {
            Some(Content::Blocks(b)) => b,
            _ => &[],
        }
    }
}

// ---------------------------------------------------------------------------
// Per-line parsing (PRD §4.4, §16)
// ---------------------------------------------------------------------------

/// What one line turned out to be. Three outcomes, no `Result`, no panic.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum LineOutcome {
    /// A modelled record, plus the untouched JSON it came from.
    Record {
        /// The typed view.
        record: Box<Record>,
        /// The raw value, which is what reaches [`TranscriptEvent::record`].
        raw: Value,
    },
    /// Valid JSON that this build does not model: an unrecognised `type`, or a
    /// known `type` whose shape changed. **Not an error** — it is the drift
    /// signal, and the payload is preserved so the drift is inspectable.
    Drift {
        /// The `type` string, if the line had a string one.
        raw_type: Option<String>,
        /// The raw value.
        raw: Value,
    },
    /// A line that is empty or whitespace. A trailing newline is not corruption.
    Blank,
    /// Not valid JSON. Routinely a half-written final line: Claude Code appends
    /// without an atomic rename, so a tailer sees torn tails.
    Corrupt,
}

/// Parse counters — the schema-drift signal PRD §16 puts in the status bar.
///
/// `BTreeMap`/`BTreeSet` rather than hash containers so the drift report is
/// byte-identical across runs and machines (PRD §7.4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParseStats {
    /// Lines that produced a modelled record.
    pub ok: u64,
    /// Empty or whitespace-only lines.
    pub blank: u64,
    /// Lines that were not valid JSON.
    pub bad_json: u64,
    /// Valid JSON whose shape did not fit the model for its `type`.
    pub bad_shape: u64,
    /// Valid JSON with a `type` this build does not know.
    pub unknown_type: u64,
    /// Lines longer than [`MAX_POLL_BYTES`], skipped so the tailer cannot stall.
    pub oversize_lines: u64,
    /// Times a followed file shrank and was re-read from offset 0.
    pub truncations: u64,
    /// Which unknown types, and how many of each.
    pub unknown_types_seen: BTreeMap<String, u64>,
    /// Every distinct CLI `version` string seen. A new one is the leading
    /// indicator that the rest of these counters are about to move.
    pub versions_seen: BTreeSet<String>,
}

impl ParseStats {
    /// Lines that did not parse cleanly, for one status-bar number.
    pub fn drift(&self) -> u64 {
        self.bad_json
            .saturating_add(self.bad_shape)
            .saturating_add(self.unknown_type)
            .saturating_add(self.oversize_lines)
    }

    /// Every line accounted for.
    pub fn total(&self) -> u64 {
        self.ok
            .saturating_add(self.blank)
            .saturating_add(self.drift())
    }

    /// Folds another counter set in, for aggregating across files.
    pub fn merge(&mut self, other: &Self) {
        self.ok += other.ok;
        self.blank += other.blank;
        self.bad_json += other.bad_json;
        self.bad_shape += other.bad_shape;
        self.unknown_type += other.unknown_type;
        self.oversize_lines += other.oversize_lines;
        self.truncations += other.truncations;
        for (k, v) in &other.unknown_types_seen {
            *self.unknown_types_seen.entry(k.clone()).or_default() += v;
        }
        self.versions_seen
            .extend(other.versions_seen.iter().cloned());
    }
}

/// Maps a wire `type` string onto [`TranscriptRecordKind`].
///
/// Written out rather than derived so the mapping is visible and so an
/// unrecognised value is [`TranscriptRecordKind::Unknown`] by construction
/// rather than by a serde attribute nobody can see.
pub fn record_kind(raw_type: &str) -> TranscriptRecordKind {
    match raw_type {
        "user" => TranscriptRecordKind::User,
        "assistant" => TranscriptRecordKind::Assistant,
        "attachment" => TranscriptRecordKind::Attachment,
        "system" => TranscriptRecordKind::System,
        "mode" => TranscriptRecordKind::Mode,
        "last-prompt" => TranscriptRecordKind::LastPrompt,
        "bridge-session" => TranscriptRecordKind::BridgeSession,
        "permission-mode" => TranscriptRecordKind::PermissionMode,
        "ai-title" => TranscriptRecordKind::AiTitle,
        "custom-title" => TranscriptRecordKind::CustomTitle,
        "agent-name" => TranscriptRecordKind::AgentName,
        "queue-operation" => TranscriptRecordKind::QueueOperation,
        "file-history-delta" => TranscriptRecordKind::FileHistoryDelta,
        "file-history-snapshot" => TranscriptRecordKind::FileHistorySnapshot,
        "started" => TranscriptRecordKind::Started,
        "result" => TranscriptRecordKind::Result,
        "atis-latch" => TranscriptRecordKind::AtisLatch,
        "frame-link" => TranscriptRecordKind::FrameLink,
        "cost-state" => TranscriptRecordKind::CostState,
        _ => TranscriptRecordKind::Unknown,
    }
}

/// Parses one JSONL line. **Total**: every `&str` returns, nothing panics, no
/// `Err` escapes.
///
/// This is the function PRD §16's fuzz target drives (through
/// [`fuzz_transcript_bytes`]). The rules it encodes:
///
/// 1. A bad line increments a counter and the caller advances the cursor past
///    it. It never aborts the file.
/// 2. An unknown `type` is **not** an error — 15 of the 19 known types are
///    cosmetic sidecars and new ones are cheap to ignore.
/// 3. The raw `Value` survives every outcome that had valid JSON, so drift is
///    inspectable rather than merely counted.
pub fn parse_line(line: &str, stats: &mut ParseStats) -> LineOutcome {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        stats.blank += 1;
        return LineOutcome::Blank;
    }
    let Ok(raw) = serde_json::from_str::<Value>(trimmed) else {
        stats.bad_json += 1;
        return LineOutcome::Corrupt;
    };
    // A line that parses but is not an object is not a record of any kind, and
    // never will be: every one of the 19 record types is a JSON object, so
    // `null`, `[]` and `123` cannot be a twentieth. Counting them as drift
    // would put an `Event` whose `record` is `null` on the bus and would make
    // every garbage line raise a `SchemaDrift` in the status bar.
    if !raw.is_object() {
        stats.bad_json += 1;
        return LineOutcome::Corrupt;
    }
    let raw_type = raw
        .get("type")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    if let Some(v) = raw.get("version").and_then(Value::as_str) {
        if !stats.versions_seen.contains(v) {
            stats.versions_seen.insert(v.to_owned());
        }
    }
    let Some(tag) = raw_type.as_deref() else {
        stats.unknown_type += 1;
        *stats
            .unknown_types_seen
            .entry("<no type>".to_owned())
            .or_default() += 1;
        return LineOutcome::Drift { raw_type, raw };
    };
    match build_record(tag, &raw) {
        Ok(Some(record)) => {
            stats.ok += 1;
            LineOutcome::Record {
                record: Box::new(record),
                raw,
            }
        }
        Ok(None) => {
            stats.unknown_type += 1;
            *stats.unknown_types_seen.entry(tag.to_owned()).or_default() += 1;
            LineOutcome::Drift { raw_type, raw }
        }
        Err(()) => {
            stats.bad_shape += 1;
            LineOutcome::Drift { raw_type, raw }
        }
    }
}

/// `Ok(Some)` modelled, `Ok(None)` unknown type, `Err` known type / wrong shape.
///
/// Deserializing from `&Value` rather than from a cloned `Value` keeps the raw
/// payload available for [`TranscriptEvent::record`] without a second copy of
/// every record.
fn build_record(tag: &str, raw: &Value) -> Result<Option<Record>, ()> {
    let kind = record_kind(tag);
    match tag {
        "user" | "assistant" | "attachment" | "system" => {
            let body: ThreadedRecord = ThreadedRecord::deserialize(raw).map_err(|_| ())?;
            let boxed = Box::new(body);
            Ok(Some(match tag {
                "user" => Record::User(boxed),
                "assistant" => Record::Assistant(boxed),
                "attachment" => Record::Attachment(boxed),
                _ => Record::System(boxed),
            }))
        }
        "started" | "result" => {
            let body: JournalRecord = JournalRecord::deserialize(raw).map_err(|_| ())?;
            Ok(Some(Record::Journal {
                kind,
                body: Box::new(body),
            }))
        }
        _ if matches!(kind, TranscriptRecordKind::Unknown) => Ok(None),
        _ => {
            let body: SidecarRecord = SidecarRecord::deserialize(raw).map_err(|_| ())?;
            Ok(Some(Record::Sidecar {
                kind,
                body: Box::new(body),
            }))
        }
    }
}

/// Splits a byte buffer into `(byte_offset, line)` pairs at `\n`, including a
/// trailing line with no newline.
///
/// Lossy UTF-8 rather than a hard failure: a torn append can cut a multi-byte
/// character in half, and that must be a skipped line, not a dead file.
fn lines_with_offsets(data: &[u8]) -> impl Iterator<Item = (u64, &[u8])> {
    let mut start = 0usize;
    std::iter::from_fn(move || {
        if start > data.len() {
            return None;
        }
        if start == data.len() {
            start += 1;
            return None;
        }
        let rest = &data[start..];
        let end = rest.iter().position(|b| *b == b'\n');
        let (line, next) = match end {
            Some(i) => (&rest[..i], start + i + 1),
            None => (rest, data.len() + 1),
        };
        let offset = start as u64;
        start = next;
        Some((offset, line))
    })
}

/// The `cargo-fuzz` entry point PRD §16 requires.
///
/// The invariant is total: this returns for every possible byte slice. Seed the
/// corpus from `tests/fixtures/transcripts/*.jsonl`, one line per input; the
/// interesting mutations are truncation (a torn append), a `"type"` whose value
/// is a number or an object rather than a string, deeply nested `input` /
/// `toolUseResult` objects, and `parentUuid` cycles.
///
/// A fuzz target is three lines:
///
/// ```ignore
/// #![no_main]
/// libfuzzer_sys::fuzz_target!(|data: &[u8]| {
///     polis_ingest::transcript::fuzz_transcript_bytes(data);
/// });
/// ```
///
/// It lives in a nightly-only `fuzz/` crate **outside** this workspace, because
/// `cargo-fuzz` needs its own manifest and toolchain.
pub fn fuzz_transcript_bytes(data: &[u8]) {
    let mut stats = ParseStats::default();
    let mapper = PathMapper::default();
    let source = TranscriptSource::Main;
    for (offset, line) in lines_with_offsets(data) {
        let text = String::from_utf8_lossy(line);
        let _ = parse_line(&text, &mut stats);
        let _ = transcript_line(&text, &source, offset, &mapper);
    }
}

// ---------------------------------------------------------------------------
// Normalisation into `Event`
// ---------------------------------------------------------------------------

/// Replaces string leaves longer than `max_chars` with their prefix plus a
/// `…[N chars, M lines elided]` marker, returning how many were elided.
///
/// The marker keeps the two numbers Polis actually reads out of long strings —
/// a line count for PRD §7.3's height, a character count for output size — while
/// bounding what a single record can cost the bus. Pass [`usize::MAX`] to
/// disable.
pub fn elide_long_strings(value: &mut Value, max_chars: usize) -> u32 {
    use std::fmt::Write as _;
    match value {
        Value::String(s) => {
            if s.chars().count() <= max_chars {
                return 0;
            }
            let chars = s.chars().count();
            let lines = s.lines().count();
            let cut = s.char_indices().nth(max_chars).map_or(s.len(), |(i, _)| i);
            let elided = chars - max_chars;
            s.truncate(cut);
            // Infallible: `fmt::Write` for `String` never returns `Err`.
            let _ = write!(s, "…[{elided} chars, {lines} lines elided]");
            1
        }
        Value::Array(items) => items
            .iter_mut()
            .map(|v| elide_long_strings(v, max_chars))
            .sum(),
        Value::Object(map) => map
            .iter_mut()
            .map(|(_, v)| elide_long_strings(v, max_chars))
            .sum(),
        _ => 0,
    }
}

/// Removes the account-identifying keys a `bridge-session` record carries.
///
/// `ownerAccountUuid` and `ownerOrganizationUuid` are the transcript's version
/// of [`polis_events::PII_ATTRIBUTES`]: Polis needs a stable agent identity, not
/// an identity document (ADR-0005). Dropped before the record reaches the bus,
/// so nothing downstream can persist them by accident.
fn scrub_pii(raw: &mut Value) {
    let Some(obj) = raw.as_object_mut() else {
        return;
    };
    for key in [
        "ownerAccountUuid",
        "ownerOrganizationUuid",
        "user.email",
        "user.account_uuid",
        "user.account_id",
    ] {
        obj.remove(key);
    }
}

/// Builds the envelope for one record.
fn meta_for(record: &Record, source: &TranscriptSource) -> EventMeta {
    let mut meta = EventMeta::now(Channel::Transcript);
    if let Some(s) = record.session_id() {
        meta = meta.with_session(SessionId::new(s));
    }
    // `agentId` on the record is authoritative; the file's own path is the
    // fallback, because 65% of subagent records elsewhere lose their sidecars
    // and a truncated record should still be attributed to its file.
    meta.worker = match (record.agent_id(), source) {
        (Some(a), _) => Some(WorkerId::new(a)),
        (None, TranscriptSource::Subagent { agent, .. }) => Some(agent.clone()),
        _ => None,
    };
    meta.agent_type = record
        .threaded()
        .and_then(|r| r.attribution_agent.as_deref())
        .map(AgentType::new);
    meta.prompt = record.prompt_id().map(PromptId::new);
    meta
}

/// Says whose session a record belongs to when the record itself does not.
///
/// A `journal.jsonl` line is `{"type","key","agentId"}` and nothing else — no
/// `sessionId`, no `toolUseId`, no `parentAgentId` — so [`meta_for`] leaves
/// [`EventMeta::session`] empty for every one of them. The file's own directory
/// is the only remaining statement of whose run it is, and it is a true one:
/// `session_files` reads the journal out of
/// `<project>/<session-id>/subagents/workflows/wf_<run>/`, so the owning session
/// is a parent directory of the path the line was read from.
///
/// This is the same fallback [`meta_for`] already applies to the *worker* id one
/// field above — *the file's own path is the fallback* — extended to the session
/// for the one record type that carries neither.
///
/// It matters because it is the difference between a workflow fleet that is
/// attributed and one that is not. Route 3 (the `wf_<runId>` directory matched
/// against the parent `Workflow` call) is the only link 503 of 643 subagent
/// transcripts have, and that parent call is a single record in the main
/// transcript: live tailing opens existing files at their end
/// ([`ProjectsTailer`] via `start_discovering`), so a Polis started after the
/// `Workflow` call will never read it, and every agent of that run parks
/// unattributed for ever with no second chance. The directory is still there.
///
/// Never an override: a record that named its own session keeps it.
fn attribute_to_file(event: &mut Event, file: &TranscriptFile) {
    if event.meta.session.is_none() {
        event.meta.session = Some(file.session.clone());
    }
}

/// Parses one JSONL line into a bus event and its display timestamp.
///
/// `byte_offset` is the record's order (ADR-0014); the record's own `timestamp`
/// is display-only. A malformed line yields `None`, never an error that aborts
/// the file (PRD §4.4). A line whose `type` is unmodelled still yields an event,
/// tagged [`TranscriptRecordKind::Unknown`] — unknown is drift, not failure.
pub fn transcript_line_parts(
    line: &str,
    source: &TranscriptSource,
    byte_offset: u64,
    stats: &mut ParseStats,
) -> Option<(Event, Option<WallTime>)> {
    let (kind, mut raw, wall, meta) = match parse_line(line, stats) {
        LineOutcome::Record { record, raw } => {
            let wall = record.timestamp().and_then(parse_timestamp);
            (record.kind(), raw, wall, meta_for(&record, source))
        }
        LineOutcome::Drift { raw, .. } => {
            let wall = raw
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_timestamp);
            let mut meta = EventMeta::now(Channel::Transcript);
            if let Some(s) = raw.get("sessionId").and_then(Value::as_str) {
                meta = meta.with_session(SessionId::new(s));
            }
            (TranscriptRecordKind::Unknown, raw, wall, meta)
        }
        LineOutcome::Blank | LineOutcome::Corrupt => return None,
    };
    scrub_pii(&mut raw);
    elide_long_strings(&mut raw, MAX_RECORD_STRING);
    let payload = Payload::Transcript(Box::new(TranscriptEvent {
        kind,
        source: source.clone(),
        byte_offset,
        record: raw,
    }));
    Some((Event::new(meta, payload), wall))
}

/// [`transcript_line_parts`] without the timestamp or the counters.
///
/// This is the signature [`crate::normalize::transcript_line`] declares; that
/// function should delegate here rather than carry a second copy of the model.
pub fn transcript_line(
    line: &str,
    source: &TranscriptSource,
    byte_offset: u64,
    mapper: &PathMapper,
) -> Option<Event> {
    // Channel D carries the record verbatim and holds no `LogicalPath` of its
    // own; paths inside a record are normalised where they are consumed, by the
    // one `PathMapper` that also serves channels A and C. The parameter stays in
    // the signature so the contract matches the other three channels.
    let _ = mapper;
    let mut stats = ParseStats::default();
    transcript_line_parts(line, source, byte_offset, &mut stats).map(|(event, _)| event)
}

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

/// Parses `2026-08-24T22:43:22.764Z`.
///
/// 123 903 of 123 903 records share that exact shape — 24 characters, always
/// millisecond precision, always `Z` — so this is a fixed-format parser rather
/// than a general ISO-8601 one: both faster and stricter. A numeric `±hh:mm`
/// offset is honoured if one ever appears. Returns `None` rather than panicking
/// on anything else; the value is display-only, so losing it costs nothing.
pub fn parse_timestamp(text: &str) -> Option<WallTime> {
    let b = text.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    if b[10] != b'T' && b[10] != b't' && b[10] != b' ' {
        return None;
    }
    let year = ascii_num(&b[0..4])?;
    let month = ascii_num(&b[5..7])?;
    let day = ascii_num(&b[8..10])?;
    let hour = ascii_num(&b[11..13])?;
    let minute = ascii_num(&b[14..16])?;
    let second = ascii_num(&b[17..19])?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let (millis, rest) = fractional_millis(&b[19..]);
    let offset_seconds = zone_offset_seconds(rest)?;
    let days = days_from_civil(year, month, day);
    let secs = days
        .checked_mul(86_400)?
        .checked_add(hour * 3600 + minute * 60 + second)?
        .checked_sub(offset_seconds)?;
    Some(WallTime::from_unix_millis(
        secs.checked_mul(1000)?.checked_add(millis)?,
    ))
}

/// Reads `.mmm…`, returning the milliseconds and the unconsumed tail.
fn fractional_millis(b: &[u8]) -> (i64, &[u8]) {
    if b.first() != Some(&b'.') {
        return (0, b);
    }
    let mut millis = 0i64;
    let mut scale = 100i64;
    let mut i = 1usize;
    while i < b.len() && b[i].is_ascii_digit() {
        if scale > 0 {
            millis += i64::from(b[i] - b'0') * scale;
            scale /= 10;
        }
        i += 1;
    }
    (millis, &b[i..])
}

/// `Z`, empty, or `±hh:mm` / `±hhmm`, in seconds east of UTC.
fn zone_offset_seconds(b: &[u8]) -> Option<i64> {
    match b.first() {
        None => Some(0),
        Some(b'Z' | b'z') if b.len() == 1 => Some(0),
        Some(sign @ (b'+' | b'-')) => {
            let sign = if *sign == b'-' { -1 } else { 1 };
            let digits: Vec<u8> = b[1..].iter().copied().filter(u8::is_ascii_digit).collect();
            if digits.len() != 4 {
                return None;
            }
            let h = ascii_num(&digits[0..2])?;
            let m = ascii_num(&digits[2..4])?;
            Some(sign * (h * 3600 + m * 60))
        }
        _ => None,
    }
}

/// All-ASCII-digit slice to `i64`, or `None`.
fn ascii_num(b: &[u8]) -> Option<i64> {
    if b.is_empty() {
        return None;
    }
    let mut n = 0i64;
    for byte in b {
        if !byte.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(i64::from(byte - b'0'))?;
    }
    Some(n)
}

/// Days since 1970-01-01 for a proleptic-Gregorian date (Howard Hinnant's
/// `days_from_civil`). Total for every input; no calendar dependency, for the
/// same reason [`WallTime`] has none (ADR-0049).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---------------------------------------------------------------------------
// Discovery: the project key, computed and discovered (ADR-0033)
// ---------------------------------------------------------------------------

/// Claude Code's 32-bit string hash, `h = (h << 5) - h + c` over **UTF-16 code
/// units**.
///
/// Three details produce a wrong directory name if guessed, all three verified
/// byte-for-byte against the live CLI:
///
/// 1. The hash is taken over the **original** `cwd`, not the munged string.
/// 2. `charCodeAt` yields UTF-16 code units, so a non-ASCII path must be hashed
///    over `encode_utf16()`, not `chars()`.
/// 3. JavaScript's `|0` is a wrapping conversion to `i32`, which is exactly what
///    `wrapping_*` gives here.
pub fn js_hash(text: &str) -> i32 {
    let mut h: i32 = 0;
    for unit in text.encode_utf16() {
        h = h
            .wrapping_shl(5)
            .wrapping_sub(h)
            .wrapping_add(i32::from(unit));
    }
    h
}

/// `Number.prototype.toString(36)` for a non-negative integer.
fn to_base36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_owned();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Computes `~/.claude/projects/<project-key>`'s directory name for a working
/// directory (PRD §4.4).
///
/// Every non-alphanumeric UTF-16 code unit becomes `-`; if the result exceeds
/// 200 code units it is cut to 200 and given a `-<base36>` suffix, where the
/// base-36 number is `Math.abs(hash(cwd))`.
///
/// **`Math.abs` on `i32::MIN` yields 2 147 483 648 in JavaScript**, which does
/// not fit in an `i32`: Rust must widen to `i64` before taking the absolute
/// value or it panics. That case is unit-tested.
///
/// The key can also be **overridden** by the CLI, so this is a *hint*: always
/// pair it with [`discover_project_dirs`] / [`index_projects`] (ADR-0033).
pub fn project_key(cwd: &str) -> String {
    let mut munged = String::with_capacity(cwd.len());
    let mut units = 0usize;
    for unit in cwd.encode_utf16() {
        let ok = matches!(unit, 0x30..=0x39 | 0x41..=0x5A | 0x61..=0x7A);
        // Every retained unit is ASCII, so the string stays byte-per-unit and
        // slicing at 200 can never split a character.
        munged.push(if ok {
            char::from_u32(u32::from(unit)).unwrap_or('-')
        } else {
            '-'
        });
        units += 1;
    }
    if units <= PROJECT_KEY_LIMIT {
        return munged;
    }
    // The widening is the whole point: `i32::MIN.abs()` panics in debug and
    // wraps in release, and JavaScript's `Math.abs` yields 2 147 483 648.
    let widened = i64::from(js_hash(cwd));
    let suffix = to_base36(widened.unsigned_abs());
    let mut key = munged;
    key.truncate(PROJECT_KEY_LIMIT);
    key.push('-');
    key.push_str(&suffix);
    key
}

/// Lists the project directories that actually exist under
/// `~/.claude/projects`, sorted.
///
/// The companion to [`project_key`]: computed keys find the common case,
/// discovery finds the ones the CLI overrode. Sorted because `read_dir` order
/// differs between NTFS, ext4 and APFS and PRD §16 compares golden files across
/// two operating systems.
pub fn discover_project_dirs(projects_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(projects_dir)? {
        let entry = entry?;
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

/// One discovered project directory, with the mapping learnt from its records.
#[derive(Debug, Clone)]
pub struct ProjectDir {
    /// Absolute path to the directory.
    pub path: PathBuf,
    /// Its name — the project key, computed or overridden.
    pub key: String,
    /// The `cwd` read out of a record inside it, when one could be read. This is
    /// the **real** mapping; the key is only how the CLI chose to spell it.
    pub cwd: Option<String>,
}

impl ProjectDir {
    /// True when the directory name is exactly what [`project_key`] computes
    /// from the `cwd` in its records.
    ///
    /// False means either the key was overridden (ADR-0033) or no `cwd` could be
    /// read. Either way the directory is still watchable — that is the whole
    /// point of discovering rather than only computing.
    pub fn is_computed(&self) -> bool {
        self.cwd
            .as_deref()
            .is_some_and(|c| project_key(c) == self.key)
    }
}

/// Discovers every project directory and learns its real `cwd` mapping.
///
/// Reads at most [`CWD_PROBE_BYTES`] from the first transcript in each
/// directory. Directories with no readable transcript are still returned, with
/// `cwd: None`: a session whose first record has not landed yet is not an error.
pub fn index_projects(projects_dir: &Path) -> io::Result<Vec<ProjectDir>> {
    let mut out = Vec::new();
    for path in discover_project_dirs(projects_dir)? {
        let key = match path.file_name().and_then(|n| n.to_str()) {
            Some(k) => k.to_owned(),
            None => continue,
        };
        let cwd = project_dir_cwd(&path);
        out.push(ProjectDir { path, key, cwd });
    }
    Ok(out)
}

/// Reads the `cwd` recorded inside a project directory's transcripts.
///
/// Absent from every sidecar record, so this scans until it finds a threaded
/// one, bounded by [`CWD_PROBE_BYTES`] per file. Returns `None` rather than an
/// error for an empty or unreadable directory.
pub fn project_dir_cwd(project_dir: &Path) -> Option<String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(project_dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    files.sort();
    for file in files {
        let Ok(bytes) = read_head(&file, CWD_PROBE_BYTES) else {
            continue;
        };
        for (_, line) in lines_with_offsets(&bytes) {
            let text = String::from_utf8_lossy(line);
            let Ok(value) = serde_json::from_str::<Value>(text.trim()) else {
                continue;
            };
            if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
                return Some(cwd.to_owned());
            }
        }
    }
    None
}

/// Finds the directory a working directory's sessions live in.
///
/// Computes [`project_key`] first — the common case, no I/O beyond one
/// `is_dir` — and falls back to reading the `cwd` out of every discovered
/// directory, which is the only thing that finds an overridden key (ADR-0033).
pub fn project_dir_for_cwd(projects_dir: &Path, cwd: &str) -> io::Result<Option<PathBuf>> {
    let computed = projects_dir.join(project_key(cwd));
    if computed.is_dir() {
        return Ok(Some(computed));
    }
    for project in index_projects(projects_dir)? {
        if project.cwd.as_deref() == Some(cwd) {
            return Ok(Some(project.path));
        }
    }
    Ok(None)
}

/// The sessions in one project directory, as `<project>/<session-id>` sidecar
/// directory paths — which is what [`session_files`] and [`SessionTailer::open`]
/// take.
///
/// Derived from the `<session-id>.jsonl` files, because the sidecar directory
/// only exists once a session spawns a subagent. Sorted.
pub fn sessions_in(project_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(project_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            if let Some(stem) = path.file_stem() {
                out.push(project_dir.join(stem));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Reads at most `limit` bytes from the front of a file.
fn read_head(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut buf = Vec::new();
    file.take(limit).read_to_end(&mut buf)?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Session file discovery
// ---------------------------------------------------------------------------

/// One transcript file discovered on disk.
#[derive(Debug, Clone)]
pub struct TranscriptFile {
    /// Absolute path. May exceed `MAX_PATH` on Windows; `std::fs` copes, but
    /// nothing may hand it to an external process without a `\\?\` prefix
    /// (ADR-0034).
    pub path: PathBuf,
    /// The session it belongs to. For a subagent file this is the **parent's**
    /// session id, which is exactly what the file itself records.
    pub session: SessionId,
    /// Which kind of file this is, with the workflow run id preserved
    /// (ADR-0047).
    pub source: TranscriptSource,
    /// Bytes already consumed. A tailer resumes from here rather than re-reading;
    /// files are append-only, so this is sound.
    pub consumed: u64,
}

impl TranscriptFile {
    /// The `agent-<id>.meta.json` beside a subagent transcript.
    ///
    /// `None` for a main transcript or a journal, which have no meta file.
    pub fn meta_path(&self) -> Option<PathBuf> {
        matches!(self.source, TranscriptSource::Subagent { .. }).then(|| meta_path_for(&self.path))
    }

    /// Sort rank: main transcript first, then subagents, then journals. Keeps
    /// replay output stable and puts the spawning `tool_use` before the file it
    /// spawned.
    fn rank(&self) -> u8 {
        match self.source {
            TranscriptSource::Main => 0,
            TranscriptSource::Subagent { .. } => 1,
            TranscriptSource::WorkflowJournal { .. } => 2,
        }
    }
}

/// Classifies a transcript path into its [`TranscriptSource`].
///
/// The `wf_<runId>` segment must be preserved: 503 of 643 subagent transcripts
/// are workflow ones and that run id is their *only* parent link (ADR-0013,
/// ADR-0047). Depth is variable, so the classification is by **filename plus the
/// nearest `wf_` ancestor**, never by counting components.
///
/// This is the same signature [`crate::normalize::transcript_source`] declares;
/// that function should delegate here.
pub fn transcript_source(path: &Path) -> Option<TranscriptSource> {
    if path.extension()? != "jsonl" {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    let run = workflow_run_of(path);
    if name == "journal.jsonl" {
        return run.map(|run| TranscriptSource::WorkflowJournal { run });
    }
    if let Some(id) = name
        .strip_prefix("agent-")
        .and_then(|r| r.strip_suffix(".jsonl"))
    {
        return Some(TranscriptSource::Subagent {
            agent: WorkerId::new(id),
            workflow_run: run,
        });
    }
    Some(TranscriptSource::Main)
}

/// The `<runId>` of the nearest `wf_<runId>` ancestor directory, if any.
fn workflow_run_of(path: &Path) -> Option<String> {
    path.ancestors()
        .filter_map(|a| a.file_name()?.to_str())
        .find_map(|n| n.strip_prefix("wf_").map(ToOwned::to_owned))
}

/// Enumerates the transcript files belonging to a session.
///
/// `session_dir` is the `<munged-cwd>/<session-id>` **sidecar directory**, which
/// need not exist: the main transcript is its sibling `<session-id>.jsonl`, and
/// a session with no subagents has no sidecar directory at all.
///
/// Globs for `agent-*.jsonl` at any depth under `subagents/`, because a workflow
/// agent sits two directories deeper than a directly spawned one, and keeps the
/// `wf_<runId>` segment (ADR-0047). Results are sorted: main first, then by path.
pub fn session_files(session_dir: &Path) -> io::Result<Vec<TranscriptFile>> {
    let Some(session_name) = session_dir.file_name().and_then(|n| n.to_str()) else {
        return Ok(Vec::new());
    };
    let session = SessionId::new(session_name);
    let mut out = Vec::new();

    let main = session_dir.with_file_name(format!("{session_name}.jsonl"));
    if main.is_file() {
        out.push(TranscriptFile {
            path: main,
            session: session.clone(),
            source: TranscriptSource::Main,
            consumed: 0,
        });
    }

    let subagents = session_dir.join("subagents");
    if subagents.is_dir() {
        let mut found = Vec::new();
        walk_jsonl(&subagents, 0, &mut found)?;
        found.sort();
        for path in found {
            let Some(source) = transcript_source(&path) else {
                continue;
            };
            if matches!(source, TranscriptSource::Main) {
                // A stray `.jsonl` under `subagents/` that is neither an agent
                // file nor a journal. Not ours; skip rather than mislabel it.
                continue;
            }
            out.push(TranscriptFile {
                path,
                session: session.clone(),
                source,
                consumed: 0,
            });
        }
    }

    out.sort_by(|a, b| a.rank().cmp(&b.rank()).then_with(|| a.path.cmp(&b.path)));
    Ok(out)
}

/// Collects `*.jsonl` under `dir`, depth-capped and order-independent.
fn walk_jsonl(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> io::Result<()> {
    if depth > MAX_SUBAGENT_DEPTH {
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            walk_jsonl(&path, depth + 1, out)?;
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Subagent attribution (§8, ADR-0013)
// ---------------------------------------------------------------------------

/// The `agent-<agentId>.meta.json` beside a subagent transcript.
///
/// **The `.jsonl` alone does not name its parent.** This file is the
/// authoritative link, 140 of 140.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMeta {
    /// `workflow-subagent` | `Explore` | `general-purpose` | `Plan` | a custom
    /// name. Present on 643 of 643.
    pub agent_type: Option<String>,
    /// Present exactly when `agentType != workflow-subagent`.
    pub description: Option<String>,
    /// The `Agent` `tool_use` that spawned this subagent. Present exactly when
    /// `agentType != workflow-subagent` (140 of 643) — resolvable to 140 of 140
    /// **only** when `tool_use` ids are indexed session-wide (§8.3).
    pub tool_use_id: Option<String>,
    /// 1, 2 or 3. Present on 643 of 643.
    pub spawn_depth: Option<u32>,
    /// Present exactly when `spawnDepth > 1`: names the parent subagent file.
    pub parent_agent_id: Option<String>,
    /// e.g. `"sonnet"`, on 5 of 643.
    pub model: Option<String>,
    /// The PRD §4.4 catch-all.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `…/agent-<id>.jsonl` → `…/agent-<id>.meta.json`.
///
/// Passing a path that is already a `.meta.json` returns it unchanged, so
/// callers may hand this either form.
pub fn meta_path_for(transcript: &Path) -> PathBuf {
    let name = transcript
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if name.ends_with(".meta.json") {
        return transcript.to_path_buf();
    }
    let stem = name.strip_suffix(".jsonl").unwrap_or(name);
    transcript.with_file_name(format!("{stem}.meta.json"))
}

/// Reads a subagent's meta file. `None` when there is none — an unattributed
/// thread, not an error.
pub fn read_agent_meta(transcript_or_meta: &Path) -> Option<AgentMeta> {
    let path = meta_path_for(transcript_or_meta);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Where a subagent's thread hangs off the session's forest.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParentLink {
    /// Spawned directly by the main transcript.
    Main,
    /// Spawned by another subagent — `spawnDepth > 1`. 23 of 643.
    Worker(WorkerId),
    /// Spawned by a `Workflow` call. 503 of 643, and these carry **neither**
    /// `toolUseId` nor `parentAgentId`: the run id matched against the parent's
    /// `toolUseResult.runId` is their only link (ADR-0047).
    Workflow {
        /// The `<runId>` from the `wf_<runId>` path segment.
        run: String,
    },
    /// No meta file and no `wf_` segment. Render it, but do not claim a parent.
    Unattributed,
}

/// Resolves a subagent transcript to its place in the forest.
///
/// Ranked by reliability, because picking the easiest route is the trap:
///
/// 1. `agent-<id>.meta.json`'s `toolUseId` — **140 of 140**, but only if
///    `tool_use` ids are indexed across *every file* of the session. Per-file
///    indexing gives 117 of 140 and looks like the format is unreliable; the 23
///    misses are nested subagents whose spawning call lives in a parent
///    *subagent* file.
/// 2. `parentAgentId`, when the meta file has one.
/// 3. The `wf_<runId>` directory segment, for workflow agents.
///
/// `promptId` is deliberately **not** used: 616 of 641 is a grouping key, not a
/// parent pointer.
pub fn resolve_parent_link(transcript_or_meta: &Path, index: &ToolUseIndex) -> ParentLink {
    let meta = read_agent_meta(transcript_or_meta);
    if let Some(meta) = &meta {
        if let Some(id) = &meta.tool_use_id {
            if let Some(site) = index.site(&ToolUseId::new(id.clone())) {
                return match &site.agent {
                    Some(agent) => ParentLink::Worker(agent.clone()),
                    None => ParentLink::Main,
                };
            }
        }
        if let Some(parent) = &meta.parent_agent_id {
            return ParentLink::Worker(WorkerId::new(parent.clone()));
        }
    }
    match workflow_run_of(transcript_or_meta) {
        Some(run) => ParentLink::Workflow { run },
        None if meta.is_some() => ParentLink::Main,
        None => ParentLink::Unattributed,
    }
}

/// Resolves a subagent to the **worker** that spawned it.
///
/// `None` means "not a subagent's child": the parent is the main transcript, a
/// workflow, or unresolved. Call [`resolve_parent_link`] when the difference
/// matters — which, for anything that draws a tree, it does.
pub fn resolve_parent(meta_json: &Path, session_index: &ToolUseIndex) -> Option<WorkerId> {
    match resolve_parent_link(meta_json, session_index) {
        ParentLink::Worker(w) => Some(w),
        _ => None,
    }
}

/// Where one `tool_use` id was seen.
#[derive(Debug, Clone)]
pub struct ToolUseSite {
    /// The file it lives in.
    pub file: PathBuf,
    /// The `uuid` of the record that issued it.
    pub record_uuid: Option<String>,
    /// The subagent that issued it, or `None` for the main transcript. This is
    /// what turns a `toolUseId` into a parent thread.
    pub agent: Option<WorkerId>,
    /// The tool's name. `Agent` for a subagent spawn; there is no `Task` tool.
    pub tool_name: Option<String>,
    /// Byte offset of the issuing record.
    pub byte_offset: u64,
}

/// A session-wide index of `tool_use` ids. **Must span every file of the
/// session**: per-file indexing resolves 117 of 140 subagents and makes the
/// format look unreliable (§8.3).
#[derive(Debug, Default, Clone)]
pub struct ToolUseIndex {
    sites: BTreeMap<String, ToolUseSite>,
    agent_spawns: BTreeMap<String, String>,
    workflow_runs: BTreeMap<String, String>,
}

impl ToolUseIndex {
    /// Indexes every file of a session in one call. The shape callers want.
    pub fn index_session(&mut self, files: &[TranscriptFile]) -> io::Result<usize> {
        let mut n = 0;
        for file in files {
            n += self.index_file(file)?;
        }
        Ok(n)
    }

    /// Adds every `tool_use` id in one file, and the `toolUseResult` back-links
    /// (route 2 and route 3 of §8.4) that confirm them.
    pub fn index_file(&mut self, file: &TranscriptFile) -> io::Result<usize> {
        let bytes = std::fs::read(&file.path)?;
        let mut stats = ParseStats::default();
        let mut added = 0usize;
        for (offset, line) in lines_with_offsets(&bytes) {
            let text = String::from_utf8_lossy(line);
            let LineOutcome::Record { record, .. } = parse_line(&text, &mut stats) else {
                continue;
            };
            added += self.index_record(&record, file, offset);
        }
        Ok(added)
    }

    fn index_record(&mut self, record: &Record, file: &TranscriptFile, offset: u64) -> usize {
        let agent = record.agent_id().map(WorkerId::new).or(match &file.source {
            TranscriptSource::Subagent { agent, .. } => Some(agent.clone()),
            _ => None,
        });
        let uuid = record.envelope().and_then(|e| e.uuid.clone());
        let mut added = 0usize;
        for block in record.blocks() {
            if block.kind.as_deref() != Some("tool_use") {
                continue;
            }
            let Some(id) = block.id.clone() else { continue };
            self.sites.insert(
                id,
                ToolUseSite {
                    file: file.path.clone(),
                    record_uuid: uuid.clone(),
                    agent: agent.clone(),
                    tool_name: block.name.clone(),
                    byte_offset: offset,
                },
            );
            added += 1;
        }
        self.index_back_links(record);
        added
    }

    /// `toolUseResult.agentId` and `.runId` — routes 2 and 3, which let a caller
    /// go from a spawned agent or a workflow run *back* to the call that made it.
    fn index_back_links(&mut self, record: &Record) {
        let Some(result) = record.threaded().and_then(|r| r.tool_use_result.as_ref()) else {
            return;
        };
        let source = record
            .threaded()
            .and_then(|r| r.source_tool_assistant_uuid.clone())
            .or_else(|| record.envelope().and_then(|e| e.parent_uuid.clone()));
        let key = record
            .blocks()
            .iter()
            .find_map(|b| b.tool_use_id.clone())
            .or(source);
        let Some(key) = key else { return };
        if let Some(agent) = result.get("agentId").and_then(Value::as_str) {
            self.agent_spawns.insert(agent.to_owned(), key.clone());
        }
        if let Some(run) = result.get("runId").and_then(Value::as_str) {
            self.workflow_runs.insert(run.to_owned(), key);
        }
    }

    /// The file a `tool_use` id was seen in.
    pub fn lookup(&self, tool_use_id: &ToolUseId) -> Option<&Path> {
        self.site(tool_use_id).map(|s| s.file.as_path())
    }

    /// Everything known about where a `tool_use` id was issued.
    pub fn site(&self, tool_use_id: &ToolUseId) -> Option<&ToolUseSite> {
        self.sites.get(tool_use_id.as_str())
    }

    /// The `tool_use` id whose result announced this subagent — route 2, which
    /// independently confirms route 1 for the 117 directly-spawned agents whose
    /// parent is the main transcript.
    pub fn spawn_of_agent(&self, agent: &WorkerId) -> Option<&ToolUseSite> {
        let id = self.agent_spawns.get(agent.as_str())?;
        self.sites.get(id)
    }

    /// The `Workflow` call that opened a run — route 3, the only link 503 of 643
    /// subagents have.
    pub fn spawn_of_workflow_run(&self, run: &str) -> Option<&ToolUseSite> {
        let id = self.workflow_runs.get(run)?;
        self.sites.get(id)
    }

    /// How many `tool_use` ids are indexed.
    pub fn len(&self) -> usize {
        self.sites.len()
    }

    /// True when nothing has been indexed.
    pub fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Tailing one file
// ---------------------------------------------------------------------------

/// Follows appends to one transcript file.
///
/// Handles the three things a naive `read_to_string` gets wrong: a half-written
/// final line (never parsed, never skipped — the cursor stops at the last `\n`),
/// truncation or rotation (the file shrinking below the cursor re-reads it from
/// 0 and counts a truncation), and a file that grows faster than one poll can
/// read (bounded by [`MAX_POLL_BYTES`], resumed on the next poll).
#[derive(Debug, Clone)]
pub struct FileTail {
    file: TranscriptFile,
}

impl FileTail {
    /// Follows `file` from its recorded `consumed` offset.
    pub fn new(file: TranscriptFile) -> Self {
        Self { file }
    }

    /// Follows `file` from its current end — live tailing, no history.
    pub fn at_end(mut file: TranscriptFile) -> Self {
        file.consumed = std::fs::metadata(&file.path).map_or(0, |m| m.len());
        Self { file }
    }

    /// The file being followed.
    pub fn file(&self) -> &TranscriptFile {
        &self.file
    }

    /// Bytes consumed so far — the resume point.
    pub fn consumed(&self) -> u64 {
        self.file.consumed
    }

    /// Resumes from a previously saved offset.
    pub fn resume_at(&mut self, offset: u64) {
        self.file.consumed = offset;
    }

    /// Reads whatever has been appended since the last poll, appending one
    /// [`Event`] per complete record to `out`.
    ///
    /// Returns how many events were appended. A file that has vanished is an
    /// `Err(NotFound)`; the caller drops the tail rather than dying.
    pub fn poll(&mut self, ctx: &mut EmitCtx<'_>, out: &mut Vec<Event>) -> io::Result<usize> {
        let mut handle = File::open(&self.file.path)?;
        let len = handle.metadata()?.len();
        if len < self.file.consumed {
            self.file.consumed = 0;
            ctx.stats.truncations += 1;
            out.push(Event::control(ControlEvent::SchemaDrift {
                channel: Channel::Transcript,
                producer_version: None,
                detail: format!(
                    "transcript shrank below the read cursor, re-reading from 0: {}",
                    self.file.path.display()
                ),
            }));
        }
        if len == self.file.consumed {
            return Ok(0);
        }
        let want = (len - self.file.consumed).min(MAX_POLL_BYTES);
        let Ok(want_usize) = usize::try_from(want) else {
            return Ok(0);
        };
        handle.seek(SeekFrom::Start(self.file.consumed))?;
        let mut buf = vec![0u8; want_usize];
        let read = read_as_much_as_possible(&mut handle, &mut buf)?;
        buf.truncate(read);

        let Some(last_newline) = buf.iter().rposition(|b| *b == b'\n') else {
            // No complete record yet. If the incomplete line already exceeds a
            // whole poll's budget it is never going to finish inside one, so
            // skip it rather than stall the tailer forever.
            if buf.len() as u64 >= MAX_POLL_BYTES {
                self.file.consumed += buf.len() as u64;
                ctx.stats.oversize_lines += 1;
            }
            return Ok(0);
        };
        let base = self.file.consumed;
        let complete = &buf[..=last_newline];
        let mut emitted = 0usize;
        for (offset, line) in lines_with_offsets(complete) {
            let text = String::from_utf8_lossy(line);
            emitted += ctx.emit(&text, &self.file, base + offset, out);
        }
        self.file.consumed = base + last_newline as u64 + 1;
        Ok(emitted)
    }
}

/// Fills `buf` as far as the file allows, tolerating a short read.
///
/// `read_exact` is wrong here: the file can shrink between `metadata` and the
/// read, and that must be a short read, not an `UnexpectedEof` that kills the
/// tail.
fn read_as_much_as_possible(handle: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match handle.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Puts a live transcript record's `observed` stamp where the record was
/// *written*, not where Polis happened to read it.
///
/// [`EventMeta::now`] stamps the moment of receipt, which is exactly right for a
/// hook datagram and exactly wrong for the [`crate::live::BACKFILL_BYTES`] of
/// history Channel D reads the instant it attaches to an already-running
/// session. Without this, opening a window replays a session the operator closed
/// twenty minutes ago as though it were happening now: `polis-world` stamps
/// `last_activity` from `observed`, calls the thread **working**, and holds a
/// rail row for a full `polis_world::THREAD_RETIRE_AFTER` — a fresh
/// thirty-minute lease granted at attach time, however old the session really
/// is. That is the "agents do not decay" bug, and it is one clock read.
///
/// # Reading a wall clock here is deliberate
///
/// [`WallTime::now`]'s docs reserve the system clock for the construction of a
/// recording, because PRD §7.4 forbids one anywhere that can reach a layout.
/// This cannot reach one: the pair is read **once, when a session is opened**,
/// and is spent entirely on [`EventMeta::observed`], which no city, layout or
/// geometry ever reads.
///
/// # A backwards step still cannot reorder the bus
///
/// 20% of transcript files step backwards and one observed jump was 60 seconds
/// (ADR-0014), so byte order remains the order and the aged stamp is passed
/// through a running maximum — the same rule `monotonise` applies to the
/// recorded path, for the same reason.
#[derive(Debug, Clone)]
pub struct Aging {
    mono_origin: Instant,
    wall_origin: WallTime,
    running: Option<Instant>,
}

impl Aging {
    /// Pairs the two clocks. One system-clock read, once per session.
    pub fn start() -> Self {
        Self {
            mono_origin: Instant::now(),
            wall_origin: WallTime::now(),
            running: None,
        }
    }

    /// Where a record written at `wall` belongs on the receipt clock.
    ///
    /// `wall` is `None` for a record that carried no timestamp — every sidecar
    /// record is one — and such a record keeps the stamp it arrived with, which
    /// is the receipt time.
    pub fn stamp(&mut self, wall: Option<WallTime>, received: Instant) -> Instant {
        let aged = wall.map_or(received, |w| self.at(w, received));
        let stamp = self.running.map_or(aged, |r| r.max(aged));
        self.running = Some(stamp);
        stamp
    }

    /// `wall` on the monotonic clock, never later than `received`: a record
    /// cannot have been written after it was read, whatever the two clocks
    /// disagree by, and a record appended since the session opened arrived
    /// within one poll of being written anyway.
    fn at(&self, wall: WallTime, received: Instant) -> Instant {
        let behind = self
            .wall_origin
            .unix_millis()
            .saturating_sub(wall.unix_millis());
        if behind <= 0 {
            // Written since the session was opened, or in the same millisecond
            // it was: the record arrived within one poll of being written, so
            // the receipt clock is already the right answer. Negative is the
            // two clocks disagreeing, and a record cannot have been written
            // after it was read.
            return received;
        }
        let behind = Duration::from_millis(u64::try_from(behind).unwrap_or(u64::MAX));
        // `checked_sub`, because `Instant`'s origin is the boot on Windows: a
        // machine that has been up for less time than the session is old has
        // nowhere further back to go, and the record keeps its receipt time.
        self.mono_origin.checked_sub(behind).unwrap_or(received)
    }
}

/// The mutable state shared by every file of one tailer while it emits.
///
/// Bundled into one struct so [`FileTail::poll`] takes one borrow rather than
/// four, and so the drift-reporting rule lives in exactly one place.
#[derive(Debug)]
pub struct EmitCtx<'a> {
    /// Parse counters for the whole session.
    pub stats: &'a mut ParseStats,
    /// Unknown `type` strings already reported, so a drifted session emits one
    /// control event per new type rather than one per line.
    pub reported: &'a mut BTreeSet<String>,
    /// The session's two clocks, so backfilled history is stamped as old as it
    /// is rather than as having just happened.
    pub age: &'a mut Aging,
}

impl EmitCtx<'_> {
    /// Parses one line and appends its event, plus a first-sighting drift signal.
    fn emit(
        &mut self,
        line: &str,
        file: &TranscriptFile,
        offset: u64,
        out: &mut Vec<Event>,
    ) -> usize {
        let before = self.stats.unknown_types_seen.len();
        let Some((mut event, wall)) = transcript_line_parts(line, &file.source, offset, self.stats)
        else {
            return 0;
        };
        attribute_to_file(&mut event, file);
        // History reads as history: see [`Aging`]. The record's own `timestamp`
        // is still verbatim inside the payload for anything that wants it.
        event.meta.observed = self.age.stamp(wall, event.meta.observed);
        if self.stats.unknown_types_seen.len() != before {
            self.report_new_drift(out);
        }
        out.push(event);
        1
    }

    /// Emits one [`ControlEvent::SchemaDrift`] per newly seen unknown type.
    fn report_new_drift(&mut self, out: &mut Vec<Event>) {
        let version = self.stats.versions_seen.iter().next_back().cloned();
        for kind in self.stats.unknown_types_seen.keys() {
            if self.reported.insert(kind.clone()) {
                out.push(Event::control(ControlEvent::SchemaDrift {
                    channel: Channel::Transcript,
                    producer_version: version.clone(),
                    detail: format!("unmodelled transcript record type {kind:?}"),
                }));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tailing one session
// ---------------------------------------------------------------------------

/// Where a newly opened tailer starts reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartAt {
    /// Offset 0 — cold-start state rebuild (PRD §4.4).
    Beginning,
    /// End of file — follow new appends only. What discovery mode uses, so that
    /// attaching to a machine with a year of history does not replay it.
    End,
}

/// Tails one session's whole file set: the main transcript plus every subagent
/// transcript and workflow journal under its sidecar directory.
///
/// Rescans on every poll, because subagent files appear at runtime — a tailer
/// that opened the file set once would silently miss four out of five records.
#[derive(Debug)]
pub struct SessionTailer {
    session_dir: PathBuf,
    session: SessionId,
    tails: BTreeMap<(u8, PathBuf), FileTail>,
    stats: ParseStats,
    reported: BTreeSet<String>,
    age: Aging,
}

impl SessionTailer {
    /// Opens a session, reading its existing content from `start`.
    ///
    /// `session_dir` is the `<munged-cwd>/<session-id>` sidecar directory; it
    /// need not exist yet.
    pub fn open(session_dir: &Path, start: StartAt) -> io::Result<Self> {
        let session = session_dir
            .file_name()
            .and_then(|n| n.to_str())
            .map_or_else(|| SessionId::new(""), SessionId::new);
        let mut tailer = Self {
            session_dir: session_dir.to_path_buf(),
            session,
            tails: BTreeMap::new(),
            stats: ParseStats::default(),
            reported: BTreeSet::new(),
            age: Aging::start(),
        };
        for file in session_files(session_dir)? {
            let key = (file.rank(), file.path.clone());
            let tail = match start {
                StartAt::Beginning => FileTail::new(file),
                StartAt::End => FileTail::at_end(file),
            };
            tailer.tails.insert(key, tail);
        }
        Ok(tailer)
    }

    /// Opens a session and resumes each file from a saved offset.
    ///
    /// Files with no saved offset start at 0 — a subagent file that appeared
    /// while Polis was not running is new work, not history to skip.
    pub fn resume(session_dir: &Path, offsets: &BTreeMap<PathBuf, u64>) -> io::Result<Self> {
        let mut tailer = Self::open(session_dir, StartAt::Beginning)?;
        for tail in tailer.tails.values_mut() {
            if let Some(offset) = offsets.get(&tail.file.path) {
                tail.resume_at(*offset);
            }
        }
        Ok(tailer)
    }

    /// The session being tailed.
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// The sidecar directory this tailer watches.
    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    /// Parse counters, including drift and truncations.
    pub fn stats(&self) -> &ParseStats {
        &self.stats
    }

    /// Every file currently followed, in emission order.
    pub fn files(&self) -> impl Iterator<Item = &TranscriptFile> {
        self.tails.values().map(FileTail::file)
    }

    /// Byte offsets to persist so a restart can [`SessionTailer::resume`].
    pub fn offsets(&self) -> BTreeMap<PathBuf, u64> {
        self.tails
            .values()
            .map(|t| (t.file.path.clone(), t.consumed()))
            .collect()
    }

    /// Picks up subagent files that appeared since the last scan. Returns how
    /// many were added. New files always start at offset 0.
    pub fn rescan(&mut self) -> io::Result<usize> {
        let mut added = 0usize;
        for file in session_files(&self.session_dir)? {
            let key = (file.rank(), file.path.clone());
            if let std::collections::btree_map::Entry::Vacant(slot) = self.tails.entry(key) {
                slot.insert(FileTail::new(file));
                added += 1;
            }
        }
        Ok(added)
    }

    /// Rescans, then reads every file's new bytes into `out`.
    ///
    /// Returns how many events were appended. Errors are absorbed per file: a
    /// transcript that was deleted mid-session drops out of the set rather than
    /// stopping the channel (ADR-0011).
    pub fn poll(&mut self, out: &mut Vec<Event>) -> usize {
        let _ = self.rescan();
        let mut emitted = 0usize;
        let mut dead = Vec::new();
        let mut ctx = EmitCtx {
            stats: &mut self.stats,
            reported: &mut self.reported,
            age: &mut self.age,
        };
        for (key, tail) in &mut self.tails {
            match tail.poll(&mut ctx, out) {
                Ok(n) => emitted += n,
                Err(e) if e.kind() == io::ErrorKind::NotFound => dead.push(key.clone()),
                Err(_) => {}
            }
        }
        for key in dead {
            self.tails.remove(&key);
        }
        emitted
    }
}

// ---------------------------------------------------------------------------
// Tailing every session under a projects directory
// ---------------------------------------------------------------------------

/// Tails every session under `~/.claude/projects`, discovering new project
/// directories and new sessions as they appear.
///
/// Discovery rather than computation is the point: the CLI computes its project
/// key as `override() ?? projectKey(cwd)`, so a session's directory is not always
/// derivable from `cwd` (ADR-0033).
#[derive(Debug)]
pub struct ProjectsTailer {
    projects_dir: PathBuf,
    sessions: BTreeMap<PathBuf, SessionTailer>,
    initial_start: StartAt,
    tick: u32,
}

impl ProjectsTailer {
    /// Opens every session currently on disk.
    ///
    /// `initial_start` applies only to what already exists; sessions and
    /// subagent files that appear afterwards are always read from 0.
    pub fn open(projects_dir: &Path, initial_start: StartAt) -> io::Result<Self> {
        let mut tailer = Self {
            projects_dir: projects_dir.to_path_buf(),
            sessions: BTreeMap::new(),
            initial_start,
            tick: 0,
        };
        tailer.scan(initial_start)?;
        Ok(tailer)
    }

    /// Picks up new project directories and new sessions. Returns how many
    /// sessions were added.
    pub fn rescan(&mut self) -> io::Result<usize> {
        self.scan(StartAt::Beginning)
    }

    fn scan(&mut self, start: StartAt) -> io::Result<usize> {
        let mut added = 0usize;
        for project in discover_project_dirs(&self.projects_dir)? {
            for session_dir in sessions_in(&project)? {
                if self.sessions.contains_key(&session_dir) {
                    continue;
                }
                if let Ok(tailer) = SessionTailer::open(&session_dir, start) {
                    self.sessions.insert(session_dir, tailer);
                    added += 1;
                }
            }
        }
        Ok(added)
    }

    /// Polls every session, rescanning for new ones every [`RESCAN_EVERY`]
    /// calls.
    ///
    /// Deterministic order: sessions in path order, files within a session
    /// main-first. The rescan is rate-limited because it costs one `read_dir`
    /// per project directory and a new *session* — unlike a new subagent file,
    /// which [`SessionTailer::poll`] catches every time — is a once-a-session
    /// event.
    pub fn poll(&mut self, out: &mut Vec<Event>) -> usize {
        if self.tick.is_multiple_of(RESCAN_EVERY) {
            let _ = self.rescan();
        }
        self.tick = self.tick.wrapping_add(1);
        self.sessions.values_mut().map(|s| s.poll(out)).sum()
    }

    /// How many sessions are being followed.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// True when no session has been discovered yet.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Where new sessions are discovered from, and where
    /// [`StartAt`] was applied.
    pub fn projects_dir(&self) -> &Path {
        &self.projects_dir
    }

    /// Aggregated parse counters across every session.
    pub fn stats(&self) -> ParseStats {
        let mut out = ParseStats::default();
        for session in self.sessions.values() {
            out.merge(session.stats());
        }
        out
    }

    /// The mode existing files were opened in, for diagnostics.
    pub fn initial_start(&self) -> StartAt {
        self.initial_start
    }
}

// ---------------------------------------------------------------------------
// Offline reading — `polis replay` (PRD §15 M2)
// ---------------------------------------------------------------------------

/// One session read offline, in the on-disk recording form (ADR-0049).
///
/// This is the milestone the PRD says to spend real time on — "read one JSONL
/// file offline and animate it over the city … most of the notation gets decided
/// in this milestone" — so it is deliberately a *value*, not a stream: it can be
/// diffed, golden-filed, and replayed twice at different speeds.
#[derive(Debug, Clone)]
pub struct Replay {
    /// Format version and wall-clock origin.
    pub header: RecordingHeader,
    /// Events in `(file, byte_offset)` order — never timestamp order.
    pub events: Vec<RecordedEvent>,
    /// What was skipped and why.
    pub stats: ParseStats,
    /// The files that were read, in the order they were read.
    pub files: Vec<TranscriptFile>,
}

impl Replay {
    /// How many events the session produced.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// True when nothing parsed.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Writes the JSON Lines recording: one [`RecordingHeader`], then one
    /// [`RecordedEvent`] per line.
    ///
    /// Deterministic: no wall clock is read, and every timestamp comes from the
    /// transcript.
    pub fn write_jsonl<W: Write>(&self, mut out: W) -> io::Result<()> {
        let header = serde_json::to_string(&self.header).map_err(io::Error::other)?;
        writeln!(out, "{header}")?;
        for event in &self.events {
            let line = event.to_json_line().map_err(io::Error::other)?;
            writeln!(out, "{line}")?;
        }
        Ok(())
    }

    /// Converts back to live events, with `observed` spaced to match the
    /// original inter-arrival gaps.
    pub fn into_live(self, clock: ReplayClock) -> Vec<Event> {
        self.events.into_iter().map(|e| clock.live(e)).collect()
    }
}

/// Reads one whole session offline: main transcript plus every subagent file.
///
/// Ordered by `(file, byte_offset)`, main transcript first, so the `Agent`
/// `tool_use` that spawned a subagent is always seen before the subagent's own
/// records — which is what a renderer needs to place the worker.
pub fn read_session(session_dir: &Path, mapper: &PathMapper) -> io::Result<Replay> {
    let files = session_files(session_dir)?;
    read_files(files, mapper)
}

/// Reads one transcript file offline.
///
/// Accepts a main transcript, a subagent transcript or a workflow journal; the
/// source is classified from the path ([`transcript_source`]), so the `wf_<runId>`
/// segment survives.
pub fn read_transcript(path: &Path, mapper: &PathMapper) -> io::Result<Replay> {
    let source = transcript_source(path).unwrap_or(TranscriptSource::Main);
    let session = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map_or_else(|| SessionId::new(""), SessionId::new);
    read_files(
        vec![TranscriptFile {
            path: path.to_path_buf(),
            session,
            source,
            consumed: 0,
        }],
        mapper,
    )
}

/// The shared body of the two offline readers.
fn read_files(files: Vec<TranscriptFile>, mapper: &PathMapper) -> io::Result<Replay> {
    let _ = mapper;
    let mut stats = ParseStats::default();
    let mut parsed: Vec<(Event, Option<WallTime>)> = Vec::new();
    for file in &files {
        let bytes = match std::fs::read(&file.path) {
            Ok(b) => b,
            // A file that vanished between discovery and read is a skipped file,
            // not a failed replay.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for (offset, line) in lines_with_offsets(&bytes) {
            let text = String::from_utf8_lossy(line);
            if let Some((mut event, wall)) =
                transcript_line_parts(&text, &file.source, offset, &mut stats)
            {
                attribute_to_file(&mut event, file);
                parsed.push((event, wall));
            }
        }
    }
    Ok(Replay {
        header: recording_header(&parsed),
        events: monotonise(parsed),
        stats,
        files,
    })
}

/// The recording's origin: the earliest timestamp anywhere in the session, so no
/// event's offset can be negative. `UNIX_EPOCH` when nothing carried a
/// timestamp — every sidecar record is timestamp-free.
fn recording_header(parsed: &[(Event, Option<WallTime>)]) -> RecordingHeader {
    let origin = parsed
        .iter()
        .filter_map(|(_, w)| *w)
        .min_by_key(|w| w.unix_millis())
        .unwrap_or(WallTime::UNIX_EPOCH);
    RecordingHeader {
        format: RECORDING_FORMAT,
        wall_origin: origin,
        producer: concat!("polis-ingest ", env!("CARGO_PKG_VERSION"), " transcript").to_owned(),
    }
}

/// Turns byte-ordered events into a recording whose `wall` and
/// `monotonic_offset_ms` sort identically (ADR-0049).
///
/// Transcript timestamps step backwards on 20% of files and one observed jump
/// was 60 seconds, so they cannot be written out as-is: a reader that sorted a
/// recording by `wall` would reorder the session. The fix is a running maximum
/// in byte order — the record's own verbatim `timestamp` is still inside
/// [`TranscriptEvent::record`] for anything that wants to show the inversion.
fn monotonise(parsed: Vec<(Event, Option<WallTime>)>) -> Vec<RecordedEvent> {
    let origin = parsed
        .iter()
        .filter_map(|(_, w)| *w)
        .map(WallTime::unix_millis)
        .min()
        .unwrap_or(0);
    let mut running = origin;
    let mut out = Vec::with_capacity(parsed.len());
    for (event, wall) in parsed {
        let millis = wall.map_or(running, WallTime::unix_millis);
        running = running.max(millis);
        let offset = u64::try_from(running.saturating_sub(origin)).unwrap_or(u64::MAX);
        out.push(RecordedEvent {
            wall: WallTime::from_unix_millis(running),
            monotonic_offset_ms: offset,
            event,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// The channel
// ---------------------------------------------------------------------------

/// What the tailer thread is following.
#[derive(Debug)]
enum Followed {
    Session(Box<SessionTailer>),
    Projects(Box<ProjectsTailer>),
    Live(Box<crate::live::LiveTailer>),
}

impl Followed {
    fn poll(&mut self, out: &mut Vec<Event>) -> usize {
        match self {
            Self::Session(t) => t.poll(out),
            Self::Projects(t) => t.poll(out),
            Self::Live(t) => t.poll(out),
        }
    }

    fn rescan(&mut self) {
        match self {
            Self::Session(_) => {}
            Self::Projects(t) => {
                let _ = t.rescan();
            }
            Self::Live(t) => t.rescan(),
        }
    }

    fn watch_root(&self) -> PathBuf {
        match self {
            // The *parent* of the sidecar directory, so the main transcript's
            // appends and the sidecar directory's creation are both covered even
            // though neither exists yet at start-up.
            Self::Session(t) => t
                .session_dir()
                .parent()
                .map_or_else(|| t.session_dir().to_path_buf(), Path::to_path_buf),
            Self::Projects(t) => t.projects_dir().to_path_buf(),
            Self::Live(t) => t.projects_dir().to_path_buf(),
        }
    }

    /// The roster, and the reason the channel is degraded, for a live watch.
    ///
    /// Published after every rescan so that the window, the status rail and
    /// `polis doctor` all read one value rather than three approximations of it.
    fn roster(&self) -> Option<crate::live::Roster> {
        match self {
            Self::Live(t) => Some(t.roster()),
            _ => None,
        }
    }
}

/// Tails one session's whole file set, or every session under a projects
/// directory (PRD §4.4).
#[derive(Debug)]
pub struct TranscriptTailer {
    stop: Arc<AtomicBool>,
    health: Arc<Mutex<SourceHealth>>,
    handle: Option<JoinHandle<()>>,
    mapper: PathMapper,
    /// Published by the thread after every rescan when this is a live watch
    /// ([`TranscriptTailer::start_live`]); `None` for the replay and
    /// single-session modes, which have no roster to publish.
    roster: Arc<Mutex<Option<crate::live::Roster>>>,
}

impl TranscriptTailer {
    /// Starts tailing a session: the main transcript plus its sidecar directory,
    /// from the beginning (a cold-start state rebuild followed by live tailing).
    pub fn start(session_dir: &Path, sink: EventSink, mapper: PathMapper) -> anyhow::Result<Self> {
        Self::start_at(session_dir, sink, mapper, StartAt::Beginning)
    }

    /// [`TranscriptTailer::start`] with explicit control over whether existing
    /// content is replayed.
    pub fn start_at(
        session_dir: &Path,
        sink: EventSink,
        mapper: PathMapper,
        start: StartAt,
    ) -> anyhow::Result<Self> {
        let tailer = SessionTailer::open(session_dir, start)?;
        Ok(Self::spawn(
            Followed::Session(Box::new(tailer)),
            sink,
            mapper,
        ))
    }

    /// Discovers and tails every session under `~/.claude/projects`.
    ///
    /// The cold-start path (PRD §4.4): **discover** directories rather than only
    /// computing them from `cwd`, because the project key can be overridden by
    /// the CLI (ADR-0033).
    ///
    /// Existing files are followed from their **end**: a machine with a year of
    /// history has hundreds of megabytes of transcripts, and replaying all of it
    /// into the bus at start-up is not a cold-start rebuild, it is a flood. To
    /// rebuild one session's state, use [`read_session`] — that is what
    /// `polis replay` does. Files that appear *after* start-up are read whole.
    pub fn start_discovering(
        projects_dir: &Path,
        sink: EventSink,
        mapper: PathMapper,
    ) -> anyhow::Result<Self> {
        let tailer = ProjectsTailer::open(projects_dir, StartAt::End)?;
        Ok(Self::spawn(
            Followed::Projects(Box::new(tailer)),
            sink,
            mapper,
        ))
    }

    /// Watches one repository's live agents, with zero configuration
    /// (PRD §15 M3).
    ///
    /// The difference from [`TranscriptTailer::start_discovering`] is what it
    /// refuses to do. That one opens a tail on every session that has ever run
    /// on the machine and follows each from its end; this one discovers the same
    /// set but follows only the sessions that are **alive and in `scope`**,
    /// catches each of them up from a bounded tail so the map is populated the
    /// instant the window opens, and publishes a [`crate::live::Roster`] saying
    /// which agents it can see — including the ones in other repositories, which
    /// are reported and deliberately not drawn.
    ///
    /// Infallible: a missing `~/.claude/projects` is a machine on which no agent
    /// has ever run. The channel reports itself degraded, with the reason, and
    /// starts watching for the directory to appear (ADR-0011).
    pub fn start_live(
        projects_dir: &Path,
        scope: crate::live::Scope,
        sink: EventSink,
        mapper: PathMapper,
    ) -> Self {
        let tailer = crate::live::LiveTailer::open(projects_dir, scope);
        Self::spawn(Followed::Live(Box::new(tailer)), sink, mapper)
    }

    /// The live roster, when this tailer was started by
    /// [`TranscriptTailer::start_live`].
    pub fn roster(&self) -> Option<crate::live::Roster> {
        match self.roster.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// The slot the roster is published into, for a caller that outlives this
    /// handle — [`crate::Ingest`] keeps one so the window can read the roster
    /// without downcasting a `Box<dyn IngestSource>`.
    pub fn roster_slot(&self) -> Arc<Mutex<Option<crate::live::Roster>>> {
        Arc::clone(&self.roster)
    }

    /// Replays a transcript offline as fast as it can be parsed (PRD §15 M2).
    ///
    /// The fastest iteration loop the project has: minutes per iteration, real
    /// data, no live infrastructure. `path` may be a single `.jsonl` file or a
    /// `<session-id>` sidecar directory, in which case the whole forest is read.
    /// Returns the number of events emitted.
    ///
    /// Ordered by `(file, byte_offset)` — never by the records' own timestamps
    /// (ADR-0014). To write the result to disk instead, call [`read_session`]
    /// and [`Replay::write_jsonl`] (ADR-0049).
    pub fn replay(path: &Path, sink: EventSink, mapper: &PathMapper) -> anyhow::Result<u64> {
        let replay = if path.is_dir() {
            read_session(path, mapper)?
        } else {
            read_transcript(path, mapper)?
        };
        let count = replay.len() as u64;
        for event in replay.into_live(ReplayClock::start_now()) {
            sink.push(event);
        }
        // The sink is consumed rather than borrowed: a replay-only run wants the
        // bus to close when the file ends, and `EventSource::recv` returning
        // `None` once every sink is dropped is how that is signalled.
        drop(sink);
        Ok(count)
    }

    /// The [`PathMapper`] this channel was started with.
    ///
    /// Channel D carries records verbatim — paths inside a record are normalised
    /// where they are consumed — so nothing in the tailer consults it. It is kept
    /// so a caller holding a `Box<dyn IngestSource>` can still report the roots
    /// the channel was configured against, which is what `polis doctor` prints.
    pub fn mapper(&self) -> &PathMapper {
        &self.mapper
    }

    fn spawn(mut followed: Followed, sink: EventSink, mapper: PathMapper) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let health = Arc::new(Mutex::new(SourceHealth::Running));
        let roster: Arc<Mutex<Option<crate::live::Roster>>> =
            Arc::new(Mutex::new(followed.roster()));
        let thread_stop = Arc::clone(&stop);
        let thread_health = Arc::clone(&health);
        let thread_roster = Arc::clone(&roster);
        let handle = std::thread::Builder::new()
            .name("polis-transcript".to_owned())
            .spawn(move || {
                let (wake_tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
                let watcher = install_watcher(&followed.watch_root(), wake_tx);
                let watcher_failed = watcher.as_ref().err().map(ToString::to_string);
                if let Some(reason) = &watcher_failed {
                    set_health(
                        &thread_health,
                        SourceHealth::Degraded {
                            reason: format!("no filesystem watcher, polling only: {reason}"),
                        },
                    );
                }
                let mut buf = Vec::new();
                let mut ticks = 0u32;
                while !thread_stop.load(Ordering::Relaxed) {
                    if ticks.is_multiple_of(RESCAN_EVERY) {
                        followed.rescan();
                        // A live watch republishes its roster on every rescan
                        // and reports "the directory an agent writes into is not
                        // there" as a degraded channel rather than as silence,
                        // which is the failure this milestone exists to fix.
                        if let Some(current) = followed.roster() {
                            let reason = current.error.clone().or_else(|| watcher_failed.clone());
                            set_health(
                                &thread_health,
                                match reason {
                                    None => SourceHealth::Running,
                                    Some(reason) => SourceHealth::Degraded { reason },
                                },
                            );
                            match thread_roster.lock() {
                                Ok(mut slot) => *slot = Some(current),
                                Err(poisoned) => *poisoned.into_inner() = Some(current),
                            }
                        }
                    }
                    ticks = ticks.wrapping_add(1);
                    buf.clear();
                    followed.poll(&mut buf);
                    for event in buf.drain(..) {
                        sink.push(event);
                    }
                    // The watcher is a wake-up hint, never a requirement: the
                    // timeout is what makes a failed watch merely degraded.
                    let _ = wake_rx.recv_timeout(POLL_INTERVAL);
                }
                drop(watcher);
                set_health(
                    &thread_health,
                    SourceHealth::Stopped {
                        reason: "shutdown requested".to_owned(),
                    },
                );
            })
            .ok();
        Self {
            stop,
            health,
            handle,
            mapper,
            roster,
        }
    }

    /// Requests shutdown and joins the tailer thread. Idempotent in effect and
    /// never panics: shutdown runs while the process is already on its way out.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for TranscriptTailer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Sets health without letting a poisoned mutex take the thread down.
fn set_health(slot: &Mutex<SourceHealth>, health: SourceHealth) {
    match slot.lock() {
        Ok(mut guard) => *guard = health,
        Err(poisoned) => *poisoned.into_inner() = health,
    }
}

/// Installs a recursive watcher that pokes the tailer loop awake.
///
/// Deliberately lossy: the channel holds one token, the payload is discarded,
/// and a full channel drops the poke — the loop is going to rescan anyway. This
/// is also why a watcher that cannot be installed is *degraded*, not fatal:
/// `notify` is untested against a `>MAX_PATH` project directory (ADR-0034).
fn install_watcher(
    root: &Path,
    wake: crossbeam_channel::Sender<()>,
) -> Result<notify::RecommendedWatcher, notify::Error> {
    use notify::Watcher as _;
    let mut watcher = notify::recommended_watcher(move |_res| {
        let _ = wake.try_send(());
    })?;
    watcher.watch(root, notify::RecursiveMode::Recursive)?;
    Ok(watcher)
}

impl IngestSource for TranscriptTailer {
    fn channel(&self) -> Channel {
        Channel::Transcript
    }

    fn health(&self) -> SourceHealth {
        match self.health.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn shutdown(self: Box<Self>) {
        (*self).shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures/transcripts");

    fn fixture(name: &str) -> PathBuf {
        Path::new(FIXTURES).join(name)
    }

    fn read_fixture(name: &str) -> String {
        std::fs::read_to_string(fixture(name)).expect("fixture must exist")
    }

    fn parse_all(text: &str) -> (Vec<Record>, ParseStats) {
        let mut stats = ParseStats::default();
        let mut records = Vec::new();
        for line in text.lines() {
            if let LineOutcome::Record { record, .. } = parse_line(line, &mut stats) {
                records.push(*record);
            }
        }
        (records, stats)
    }

    // -- the model -------------------------------------------------------

    #[test]
    fn every_fixture_parses_with_zero_drift() {
        for name in [
            "main-session-mixed.jsonl",
            "main-session-with-subagent.jsonl",
            "subagent-a96cf8a57447af436.jsonl",
        ] {
            let text = read_fixture(name);
            let expected = text.lines().filter(|l| !l.trim().is_empty()).count() as u64;
            let (records, stats) = parse_all(&text);
            assert_eq!(stats.bad_json, 0, "{name}: bad json");
            assert_eq!(stats.bad_shape, 0, "{name}: bad shape — {stats:?}");
            assert_eq!(
                stats.unknown_type, 0,
                "{name}: unknown types {:?}",
                stats.unknown_types_seen
            );
            assert_eq!(stats.ok, expected, "{name}: not every line modelled");
            assert_eq!(records.len() as u64, expected);
        }
    }

    /// ADR-0035: with every field `Option` and a `flatten` catch-all, a missing
    /// `rename_all` makes serde silently return `None` for everything
    /// interesting and report **no error at all**. Parse success is therefore
    /// not the assertion; population is.
    #[test]
    fn the_fields_polis_depends_on_actually_deserialize() {
        let text = read_fixture("main-session-mixed.jsonl");
        let (records, _) = parse_all(&text);
        let count = |f: &dyn Fn(&Record) -> bool| records.iter().filter(|r| f(r)).count();

        assert!(count(&|r| r.session_id().is_some()) > 0, "sessionId");
        assert!(count(&|r| r.prompt_id().is_some()) > 0, "promptId");
        assert!(count(&|r| r.version().is_some()) > 0, "version");
        assert!(count(&|r| r.timestamp().is_some()) > 0, "timestamp");
        assert!(
            count(&|r| r.threaded().is_some_and(|t| t.tool_use_result.is_some())) > 0,
            "toolUseResult"
        );
        assert!(
            count(&|r| r.threaded().is_some_and(|t| t.request_id.is_some())) > 0,
            "requestId"
        );
        assert!(
            count(&|r| r
                .threaded()
                .is_some_and(|t| t.source_tool_assistant_uuid.is_some()))
                > 0,
            "sourceToolAssistantUUID needs an explicit rename, not camelCase"
        );
        assert!(
            records.iter().any(|r| !r.blocks().is_empty()),
            "message.content blocks"
        );
        // `sourceToolAssistantUUID == parentUuid` on 37 912 of 37 912 records.
        for r in &records {
            let (Some(t), Some(e)) = (r.threaded(), r.envelope()) else {
                continue;
            };
            if let Some(src) = &t.source_tool_assistant_uuid {
                assert_eq!(
                    Some(src),
                    e.parent_uuid.as_ref(),
                    "the free integrity check"
                );
            }
        }
    }

    #[test]
    fn a_quarter_of_the_corpus_has_no_uuid_and_that_is_not_an_error() {
        let text = read_fixture("main-session-mixed.jsonl");
        let (records, stats) = parse_all(&text);
        let uuidless = records
            .iter()
            .filter(|r| r.envelope().is_none_or(|e| e.uuid.is_none()))
            .count();
        assert!(uuidless > 0, "the mixed fixture carries flat sidecars");
        assert_eq!(stats.bad_shape, 0, "sidecars are records, not failures");
        // Every sidecar is a record with a kind and no envelope at all.
        let sidecars: Vec<_> = records
            .iter()
            .filter(|r| matches!(r, Record::Sidecar { .. }))
            .collect();
        assert!(sidecars.len() >= 15, "10 record types, 15+ sidecar lines");
        let mut named = 0usize;
        for s in sidecars {
            assert!(s.envelope().is_none());
            assert!(!matches!(s.kind(), TranscriptRecordKind::Unknown));
            // Most sidecars name their session — but not all: the two
            // `file-history-*` types carry only a `messageId`, so a tailer that
            // required a session id would drop them.
            if s.session_id().is_some() {
                named += 1;
            } else {
                assert_eq!(s.kind(), TranscriptRecordKind::FileHistorySnapshot);
            }
        }
        assert!(named > 0);
    }

    #[test]
    fn the_two_session_id_spellings_are_kept_apart() {
        let line = r#"{"type":"user","sessionId":"self","session_id":"ancestor",
            "message":{"role":"user","content":"hi"}}"#;
        let mut stats = ParseStats::default();
        let LineOutcome::Record { record, .. } = parse_line(line, &mut stats) else {
            panic!("must parse");
        };
        let env = record.envelope().expect("threaded");
        assert_eq!(env.session_id.as_deref(), Some("self"));
        assert_eq!(
            env.ancestor_session_id.as_deref(),
            Some("ancestor"),
            "snake session_id is the resumed-from lineage, not a duplicate"
        );
    }

    #[test]
    fn user_content_may_be_a_bare_string() {
        let line = r#"{"type":"user","sessionId":"s","message":{"role":"user","content":"hello"}}"#;
        let mut stats = ParseStats::default();
        let LineOutcome::Record { record, .. } = parse_line(line, &mut stats) else {
            panic!("must parse");
        };
        let content = record
            .threaded()
            .and_then(|t| t.message.as_ref())
            .and_then(|m| m.content.as_ref());
        assert!(matches!(content, Some(Content::Text(t)) if t == "hello"));
        assert!(record.blocks().is_empty());
    }

    // -- one bad line never aborts the file ------------------------------

    #[test]
    fn corrupt_lines_do_not_cost_a_single_good_record() {
        let good = read_fixture("main-session-with-subagent.jsonl");
        let good_lines: Vec<&str> = good.lines().filter(|l| !l.trim().is_empty()).collect();

        let mut poisoned = String::new();
        poisoned.push_str("{ this is not json\n");
        for (i, line) in good_lines.iter().enumerate() {
            poisoned.push_str(line);
            poisoned.push('\n');
            match i % 5 {
                0 => poisoned.push_str("{\"type\":\n"),
                1 => poisoned.push('\n'),
                2 => poisoned.push_str("{\"type\":123,\"x\":1}\n"),
                3 => poisoned.push_str("{\"type\":\"brand-new-in-2-1-999\",\"x\":1}\n"),
                _ => poisoned.push_str("[1,2,3]\n"),
            }
        }
        // A torn final append: valid prefix, no newline, no closing brace.
        poisoned.push_str("{\"type\":\"user\",\"sessionId\":\"x\"");

        let (records, stats) = parse_all(&poisoned);
        assert_eq!(
            stats.ok,
            good_lines.len() as u64,
            "every good record survived: {stats:?}"
        );
        assert_eq!(records.len(), good_lines.len());
        assert!(stats.bad_json > 0, "the corrupt lines were counted");
        assert!(stats.unknown_type > 0, "unknown types were counted");
        assert!(
            stats
                .unknown_types_seen
                .contains_key("brand-new-in-2-1-999"),
            "drift names the type it saw: {:?}",
            stats.unknown_types_seen
        );
        assert!(stats.drift() > 0);
    }

    #[test]
    fn an_unknown_type_still_produces_an_event_because_unknown_is_drift_not_failure() {
        let line = r#"{"type":"quantum-latch","sessionId":"s","x":1}"#;
        let event = transcript_line(line, &TranscriptSource::Main, 42, &PathMapper::default())
            .expect("drift still emits");
        let Payload::Transcript(t) = &event.payload else {
            panic!("wrong payload");
        };
        assert_eq!(t.kind, TranscriptRecordKind::Unknown);
        assert_eq!(t.byte_offset, 42);
        assert_eq!(t.record.get("x").and_then(Value::as_i64), Some(1));
        assert_eq!(
            event.meta.session.as_ref().map(SessionId::as_str),
            Some("s")
        );
    }

    #[test]
    fn corrupt_and_blank_lines_produce_no_event() {
        let m = PathMapper::default();
        assert!(transcript_line("", &TranscriptSource::Main, 0, &m).is_none());
        assert!(transcript_line("   \t ", &TranscriptSource::Main, 0, &m).is_none());
        assert!(transcript_line("{oops", &TranscriptSource::Main, 0, &m).is_none());
    }

    #[test]
    fn parse_line_is_total_over_awkward_input() {
        let mut stats = ParseStats::default();
        for line in [
            "",
            " ",
            "\u{0}",
            "null",
            "123",
            "\"a string\"",
            "[]",
            "{}",
            r#"{"type":null}"#,
            r#"{"type":{"nested":true}}"#,
            r#"{"type":"user","message":5}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_use"}]}}"#,
            r#"{"type":"assistant","uuid":"\ud800"}"#,
        ] {
            let _ = parse_line(line, &mut stats);
        }
        assert_eq!(stats.total(), 13, "every line was accounted for: {stats:?}");
    }

    #[test]
    fn the_fuzz_entry_point_returns_for_arbitrary_bytes() {
        fuzz_transcript_bytes(b"");
        fuzz_transcript_bytes(b"\n\n\n");
        fuzz_transcript_bytes(&[0xff, 0xfe, b'\n', 0x00]);
        fuzz_transcript_bytes(read_fixture("main-session-mixed.jsonl").as_bytes());
        // A torn multi-byte character across the read boundary.
        let text = read_fixture("subagent-a96cf8a57447af436.jsonl");
        for cut in [1usize, 7, 100, 1000] {
            fuzz_transcript_bytes(&text.as_bytes()[..cut.min(text.len())]);
        }
    }

    // -- the munged-cwd rule ---------------------------------------------

    /// Every pair here was read off the real `~/.claude/projects` on the machine
    /// `docs/verified/jsonl-schema.md` was written on: the `cwd` comes out of a
    /// record inside the directory, the key is the directory's own name.
    const REAL_PROJECT_DIRS: &[(&str, &str)] = &[
        (r"C:\coding\agentolis", "C--coding-agentolis"),
        (r"C:\coding", "C--coding"),
        (r"C:\coding\biwt", "C--coding-biwt"),
        (r"C:\Users\konka", "C--Users-konka"),
        (r"C:\coding\qurio-toolset", "C--coding-qurio-toolset"),
        (
            r"C:\coding\stickingplacebooks",
            "C--coding-stickingplacebooks",
        ),
        (
            r"C:\Users\konka\AppData\Local\Temp\claude\C--coding-agentolis",
            "C--Users-konka-AppData-Local-Temp-claude-C--coding-agentolis",
        ),
    ];

    #[test]
    fn project_key_reproduces_the_real_directory_names() {
        for (cwd, dir) in REAL_PROJECT_DIRS {
            assert_eq!(&project_key(cwd), dir, "cwd {cwd}");
            assert!(project_key(cwd).len() <= 200, "short paths get no suffix");
        }
    }

    /// The live-CLI experiment from `docs/verified/jsonl-schema.md` §1.1: a
    /// 218-character `cwd` produced a 207-character directory whose base-36
    /// suffix is `w6jyaf`. Predicted and actual matched byte-for-byte.
    #[test]
    fn project_key_reproduces_the_hashed_suffix_verified_against_the_live_cli() {
        let cwd = concat!(
            r"C:\Users\konka\AppData\Local\Temp\claude\C--coding-agentolis",
            r"\6f51089f-1ec0-4e78-9bc9-8ff6864e0100\scratchpad",
            r"\aaaaaaaaaa\bbbbbbbbbb\cccccccccc\dddddddddd\eeeeeeeeee",
            r"\ffffffffff\gggggggggg\hhhhhhhhhh\iiiiiiiiii\jjjjjjjjjj"
        );
        assert_eq!(cwd.chars().count(), 218);
        let key = project_key(cwd);
        assert_eq!(key.len(), 207);
        assert!(key.ends_with("-w6jyaf"), "{key}");
        assert!(
            key.starts_with("C--Users-konka-AppData-Local-Temp-claude-C--coding-agentolis-"),
            "{key}"
        );
        // The hash is over the ORIGINAL cwd, not the munged string.
        let munged: String = cwd
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        assert_ne!(js_hash(cwd), js_hash(&munged));
    }

    /// `Math.abs(-2147483648)` is `2147483648` in JavaScript, which does not fit
    /// in an `i32`. `i32::MIN.abs()` panics in debug and wraps in release, so
    /// the widening to `i64` is the difference between a correct suffix and a
    /// crash on a real path. This `cwd` was solved for: its hash is exactly
    /// `i32::MIN`.
    #[test]
    fn the_hash_widens_to_i64_because_abs_of_i32_min_does_not_fit() {
        let cwd = concat!(
            r"C:\deep\segment01\segment02\segment03\segment04\segment05\segment06",
            r"\segment07\segment08\segment09\segment10\segment11\segment12",
            r"\segment13\segment14\segment15\segment16\segment17\segment18",
            r"\segment19\segment20\segment21\segment22\1B;G4K8"
        );
        assert_eq!(js_hash(cwd), i32::MIN, "the solved-for input");
        assert!(cwd.chars().count() > 200, "long enough to need a suffix");
        let key = project_key(cwd);
        // 2 147 483 648 in base 36.
        assert!(key.ends_with("-zik0zk"), "{key}");
        assert_eq!(key.len(), 207);
    }

    #[test]
    fn the_hash_is_over_utf16_code_units_not_chars() {
        // U+1F600 is one `char` and two UTF-16 code units, so a `chars()`-based
        // hash disagrees here and only here.
        let with_astral = "C:\\a\u{1F600}b";
        let mut chars_hash: i32 = 0;
        for c in with_astral.chars() {
            chars_hash = chars_hash
                .wrapping_shl(5)
                .wrapping_sub(chars_hash)
                .wrapping_add(c as i32);
        }
        assert_ne!(js_hash(with_astral), chars_hash);
        // And the astral char munges to two dashes, one per code unit.
        assert_eq!(project_key(with_astral), "C--a--b");
    }

    #[test]
    fn project_key_is_total() {
        for cwd in ["", "/", "a", "\u{0}", &"x".repeat(5000), "日本語のパス"] {
            let key = project_key(cwd);
            assert!(key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
            assert!(key.len() <= 200 + 1 + 13);
        }
    }

    /// Opportunistic: when the machine that produced `docs/verified` is the one
    /// running the test, check every real directory rather than the seven
    /// hard-coded pairs. Skipped everywhere else rather than failing.
    #[test]
    fn every_discoverable_project_directory_round_trips() {
        let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
        else {
            return;
        };
        let projects = Path::new(&home).join(".claude").join("projects");
        let Ok(dirs) = index_projects(&projects) else {
            return;
        };
        if dirs.is_empty() {
            return;
        }
        let mut checked = 0usize;
        for dir in &dirs {
            let Some(cwd) = &dir.cwd else { continue };
            assert_eq!(
                &project_key(cwd),
                &dir.key,
                "computed key disagrees for {cwd}"
            );
            assert!(dir.is_computed());
            checked += 1;
        }
        assert!(checked > 0, "at least one directory yielded a cwd");
    }

    // -- discovery -------------------------------------------------------

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn discovery_finds_directories_the_computed_key_would_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path();

        // One directory whose name is what `project_key` computes...
        let computed = projects.join(project_key(r"C:\coding\agentolis"));
        write(
            &computed.join("11111111-1111-1111-1111-111111111111.jsonl"),
            "{\"type\":\"mode\",\"mode\":\"normal\",\"sessionId\":\"s\"}\n\
             {\"type\":\"user\",\"sessionId\":\"s\",\"cwd\":\"C:\\\\coding\\\\agentolis\",\
             \"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        );
        // ...and one the CLI overrode, which no amount of computing will find.
        let overridden = projects.join("my-custom-key");
        write(
            &overridden.join("22222222-2222-2222-2222-222222222222.jsonl"),
            "{\"type\":\"user\",\"sessionId\":\"t\",\"cwd\":\"C:\\\\elsewhere\",\
             \"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        );

        let dirs = index_projects(projects).unwrap();
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].path, computed, "sorted, so deterministic");
        assert!(dirs[0].is_computed());

        let overridden_entry = dirs.iter().find(|d| d.key == "my-custom-key").unwrap();
        assert_eq!(overridden_entry.cwd.as_deref(), Some(r"C:\elsewhere"));
        assert!(
            !overridden_entry.is_computed(),
            "an overridden key is not derivable from cwd — ADR-0033"
        );

        // And the reverse lookup works for both.
        assert_eq!(
            project_dir_for_cwd(projects, r"C:\coding\agentolis").unwrap(),
            Some(computed)
        );
        assert_eq!(
            project_dir_for_cwd(projects, r"C:\elsewhere").unwrap(),
            Some(overridden.clone()),
            "found only by reading cwd out of the records"
        );
        assert_eq!(project_dir_for_cwd(projects, r"C:\nowhere").unwrap(), None);
    }

    // -- session layout --------------------------------------------------

    /// Builds the real on-disk shape from the checked-in fixtures:
    /// `<project>/<sid>.jsonl` plus `<project>/<sid>/subagents/agent-<id>.jsonl`
    /// and its `.meta.json`.
    fn build_session(root: &Path) -> (PathBuf, String) {
        let sid = "005d9938-2b44-45f6-87ac-4cddac3b0d6b".to_owned();
        let project = root.join("C--coding-demo");
        write(
            &project.join(format!("{sid}.jsonl")),
            &read_fixture("main-session-with-subagent.jsonl"),
        );
        let agents = project.join(&sid).join("subagents");
        write(
            &agents.join("agent-a96cf8a57447af436.jsonl"),
            &read_fixture("subagent-a96cf8a57447af436.jsonl"),
        );
        write(
            &agents.join("agent-a96cf8a57447af436.meta.json"),
            &read_fixture("subagent-a96cf8a57447af436.meta.json"),
        );
        (project.join(&sid), sid)
    }

    #[test]
    fn session_files_finds_the_forest_not_just_the_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let (session_dir, sid) = build_session(tmp.path());
        let files = session_files(&session_dir).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].source, TranscriptSource::Main, "main sorts first");
        assert_eq!(files[0].session.as_str(), sid);
        assert_eq!(
            files[1].source,
            TranscriptSource::Subagent {
                agent: WorkerId::new("a96cf8a57447af436"),
                workflow_run: None,
            }
        );
        assert_eq!(
            files[1].session.as_str(),
            sid,
            "a subagent file records the PARENT's session id"
        );
        assert!(files[1].meta_path().unwrap().is_file());
        assert!(files[0].meta_path().is_none());
    }

    #[test]
    fn a_session_with_no_subagents_still_yields_its_main_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        write(&project.join("abc.jsonl"), "{\"type\":\"mode\"}\n");
        let files = session_files(&project.join("abc")).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].source, TranscriptSource::Main);
        // And a session that does not exist at all is empty, not an error.
        assert!(session_files(&project.join("nope")).unwrap().is_empty());
    }

    #[test]
    fn workflow_subagents_keep_the_run_id_that_is_their_only_parent_link() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let sid = "sess";
        write(&project.join("sess.jsonl"), "{\"type\":\"mode\"}\n");
        let run_dir = project
            .join(sid)
            .join("subagents")
            .join("workflows")
            .join("wf_run_2026");
        write(&run_dir.join("agent-a1234.jsonl"), "{\"type\":\"mode\"}\n");
        write(
            &run_dir.join("journal.jsonl"),
            "{\"type\":\"started\",\"key\":\"v2:aa\",\"agentId\":\"a1234\"}\n",
        );

        let files = session_files(&project.join(sid)).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[1].source,
            TranscriptSource::Subagent {
                agent: WorkerId::new("a1234"),
                workflow_run: Some("run_2026".to_owned()),
            },
            "dropping the run id orphans four out of five subagents — ADR-0047"
        );
        assert_eq!(
            files[2].source,
            TranscriptSource::WorkflowJournal {
                run: "run_2026".to_owned()
            }
        );
    }

    #[test]
    fn a_journal_line_carries_the_session_of_the_directory_it_was_read_from() {
        // A journal record is `{type, key, agentId}` — no `sessionId`, so
        // `meta_for` leaves the session empty and the world has nothing to
        // attribute the agent to but the parent `Workflow` call, which live
        // tailing has usually already skipped past. The path still knows.
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let sid = "sess";
        write(&project.join("sess.jsonl"), "{\"type\":\"mode\"}\n");
        let line = "{\"type\":\"started\",\"key\":\"v2:aa\",\"agentId\":\"a1234\"}\n";
        write(
            &project
                .join(sid)
                .join("subagents")
                .join("workflows")
                .join("wf_run_2026")
                .join("journal.jsonl"),
            line,
        );
        let files = session_files(&project.join(sid)).unwrap();
        let journal = files
            .iter()
            .find(|f| matches!(f.source, TranscriptSource::WorkflowJournal { .. }))
            .expect("the journal");

        let mut stats = ParseStats::default();
        let (mut event, _) =
            transcript_line_parts(line.trim_end(), &journal.source, 0, &mut stats).expect("parsed");
        assert!(
            event.meta.session.is_none(),
            "the record itself names no session — that is the whole problem"
        );
        attribute_to_file(&mut event, journal);
        assert_eq!(
            event.meta.session,
            Some(SessionId::new(sid)),
            "so the directory it was read out of says it instead"
        );
    }

    #[test]
    fn a_record_that_names_its_own_session_is_never_overridden_by_the_path() {
        // The fallback is a fallback. A subagent transcript carries the parent's
        // `sessionId`, and that is a stronger statement than the directory —
        // for a worktree or a moved sidecar the two can disagree.
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        write(&project.join("outer.jsonl"), "{\"type\":\"mode\"}\n");
        let line = "{\"type\":\"assistant\",\"sessionId\":\"inner\",\"message\":{\"content\":[]}}";
        write(
            &project
                .join("outer")
                .join("subagents")
                .join("agent-a1234.jsonl"),
            "{\"type\":\"mode\"}\n",
        );
        let files = session_files(&project.join("outer")).unwrap();
        let sub = files
            .iter()
            .find(|f| matches!(f.source, TranscriptSource::Subagent { .. }))
            .expect("the subagent file");

        let mut stats = ParseStats::default();
        let (mut event, _) =
            transcript_line_parts(line, &sub.source, 0, &mut stats).expect("parsed");
        attribute_to_file(&mut event, sub);
        assert_eq!(
            event.meta.session,
            Some(SessionId::new("inner")),
            "the record's own word wins"
        );
    }

    #[test]
    fn transcript_source_never_assumes_a_fixed_depth() {
        let deep = Path::new("/x/sid/subagents/workflows/wf_r/nested/agent-abc.jsonl");
        assert_eq!(
            transcript_source(deep),
            Some(TranscriptSource::Subagent {
                agent: WorkerId::new("abc"),
                workflow_run: Some("r".to_owned()),
            })
        );
        assert_eq!(
            transcript_source(Path::new("/x/sid.jsonl")),
            Some(TranscriptSource::Main)
        );
        assert_eq!(transcript_source(Path::new("/x/sid.meta.json")), None);
        assert_eq!(
            transcript_source(Path::new("/x/journal.jsonl")),
            None,
            "a journal outside a wf_ directory has no run id to name it by"
        );
    }

    // -- subagent attribution --------------------------------------------

    #[test]
    fn the_meta_json_chain_resolves_the_subagent_to_its_spawning_tool_use() {
        let tmp = tempfile::tempdir().unwrap();
        let (session_dir, _) = build_session(tmp.path());
        let files = session_files(&session_dir).unwrap();

        let mut index = ToolUseIndex::default();
        let n = index.index_session(&files).unwrap();
        assert!(n > 0, "the session issued tool calls");

        // §8.3's chain, end to end.
        let meta = read_agent_meta(&files[1].path).expect("the meta file is the parent link");
        assert_eq!(meta.agent_type.as_deref(), Some("general-purpose"));
        assert_eq!(meta.spawn_depth, Some(1));
        let tool_use_id = ToolUseId::new(meta.tool_use_id.clone().unwrap());
        assert_eq!(tool_use_id.as_str(), "toolu_01Q3g2QgvKmHqJZ3XMPgmJJ5");

        let site = index.site(&tool_use_id).expect("indexed session-wide");
        assert_eq!(
            site.tool_name.as_deref(),
            Some("Agent"),
            "not `Task` — §14.7"
        );
        assert_eq!(site.file, files[0].path, "spawned from the main transcript");
        assert!(site.agent.is_none(), "the main transcript has no agentId");
        assert_eq!(index.lookup(&tool_use_id), Some(files[0].path.as_path()));

        // Route 2 independently confirms route 1.
        let agent = WorkerId::new("a96cf8a57447af436");
        let confirm = index.spawn_of_agent(&agent).expect("toolUseResult.agentId");
        assert_eq!(confirm.tool_name.as_deref(), Some("Agent"));

        assert_eq!(
            resolve_parent_link(&files[1].path, &index),
            ParentLink::Main
        );
        assert_eq!(
            resolve_parent(&files[1].meta_path().unwrap(), &index),
            None,
            "None means `not a subagent's child`, and the parent here is main"
        );
    }

    #[test]
    fn a_nested_subagent_resolves_to_the_worker_that_spawned_it() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("p");
        let sid = "s";
        write(&project.join("s.jsonl"), "{\"type\":\"mode\"}\n");
        let agents = project.join(sid).join("subagents");
        // A depth-1 agent whose transcript issues the `Agent` call...
        write(
            &agents.join("agent-aparent.jsonl"),
            "{\"type\":\"assistant\",\"sessionId\":\"s\",\"agentId\":\"aparent\",\
             \"uuid\":\"u1\",\"parentUuid\":null,\"isSidechain\":true,\
             \"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\
             \"id\":\"toolu_nested\",\"name\":\"Agent\",\"input\":{}}]}}\n",
        );
        write(
            &agents.join("agent-aparent.meta.json"),
            "{\"agentType\":\"general-purpose\",\"spawnDepth\":1,\"toolUseId\":\"toolu_top\"}",
        );
        // ...and the depth-2 agent it spawned.
        write(&agents.join("agent-achild.jsonl"), "{\"type\":\"mode\"}\n");
        write(
            &agents.join("agent-achild.meta.json"),
            "{\"agentType\":\"Explore\",\"spawnDepth\":2,\"toolUseId\":\"toolu_nested\",\
             \"parentAgentId\":\"aparent\"}",
        );

        let files = session_files(&project.join(sid)).unwrap();
        let mut index = ToolUseIndex::default();
        index.index_session(&files).unwrap();

        let child = files
            .iter()
            .find(|f| f.path.ends_with("agent-achild.jsonl"))
            .unwrap();
        assert_eq!(
            resolve_parent_link(&child.path, &index),
            ParentLink::Worker(WorkerId::new("aparent")),
            "per-file indexing would miss this — the spawning call is in a SUBAGENT file"
        );
        assert_eq!(
            resolve_parent(&child.meta_path().unwrap(), &index),
            Some(WorkerId::new("aparent"))
        );
    }

    #[test]
    fn a_workflow_subagent_with_no_meta_file_links_by_its_run_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("subagents/workflows/wf_abc123/agent-a9.jsonl");
        write(&path, "{\"type\":\"mode\"}\n");
        assert_eq!(
            resolve_parent_link(&path, &ToolUseIndex::default()),
            ParentLink::Workflow {
                run: "abc123".to_owned()
            }
        );
        // No meta, no run directory: unattributed, but never an error.
        let orphan = tmp.path().join("subagents/agent-a8.jsonl");
        write(&orphan, "{\"type\":\"mode\"}\n");
        assert_eq!(
            resolve_parent_link(&orphan, &ToolUseIndex::default()),
            ParentLink::Unattributed
        );
    }

    #[test]
    fn attribution_agent_is_a_type_and_never_an_id() {
        let text = read_fixture("subagent-a96cf8a57447af436.jsonl");
        let (records, _) = parse_all(&text);
        let mut seen = 0;
        for r in &records {
            let Some(t) = r.threaded() else { continue };
            let (Some(kind), Some(id)) = (&t.attribution_agent, r.agent_id()) else {
                continue;
            };
            assert_ne!(kind, id, "attributionAgent is the agent TYPE — §8.5");
            assert_eq!(kind, "general-purpose");
            seen += 1;
        }
        assert!(seen > 0, "the subagent fixture carries attributionAgent");
    }

    #[test]
    fn a_subagent_record_is_attributed_even_when_its_agent_id_is_missing() {
        let source = TranscriptSource::Subagent {
            agent: WorkerId::new("a1"),
            workflow_run: None,
        };
        // No `agentId` on the record — the file's own path still attributes it.
        let line = r#"{"type":"user","sessionId":"s","message":{"role":"user","content":"x"}}"#;
        let event = transcript_line(line, &source, 0, &PathMapper::default()).unwrap();
        assert!(event.meta.is_worker());
        assert_eq!(event.meta.worker.as_ref().map(WorkerId::as_str), Some("a1"));
        // And the thread key is the SESSION, never the agent (a subagent file
        // records the parent's session id).
        assert_eq!(
            event
                .meta
                .thread
                .as_ref()
                .map(polis_events::ThreadId::as_str),
            Some("s")
        );
    }

    // -- tailing ---------------------------------------------------------

    fn poll_count(tailer: &mut SessionTailer) -> (usize, Vec<Event>) {
        let mut out = Vec::new();
        let n = tailer.poll(&mut out);
        (n, out)
    }

    #[test]
    fn backfilled_history_is_stamped_as_old_as_it_is() {
        // The bug this exists for: Channel D reads up to `live::BACKFILL_BYTES`
        // of an already-running session the instant it attaches, and
        // `EventMeta::now` stamped every one of those records "now". A session
        // the operator closed twenty minutes ago therefore arrived in
        // `polis-world` as `working`, holding a rail row on a fresh
        // `THREAD_RETIRE_AFTER` lease — which is the whole "agents never decay"
        // report.
        let mut age = Aging::start();
        let received = Instant::now();
        let minute_ago = WallTime::from_unix_millis(WallTime::now().unix_millis() - 60_000);

        let stamped = age.stamp(Some(minute_ago), received);
        assert!(
            received.saturating_duration_since(stamped) >= Duration::from_secs(55),
            "a record written a minute ago must read as a minute old, not as now"
        );
    }

    #[test]
    fn a_backwards_timestamp_step_still_cannot_reorder_the_bus() {
        // 20% of transcript files step backwards and one observed jump was 60
        // seconds (ADR-0014). Byte order is the order, so the aged stamp goes
        // through a running maximum — exactly what `monotonise` does for the
        // recorded path.
        let mut age = Aging::start();
        let received = Instant::now();
        let now = WallTime::now().unix_millis();

        let first = age.stamp(Some(WallTime::from_unix_millis(now - 60_000)), received);
        let second = age.stamp(Some(WallTime::from_unix_millis(now - 600_000)), received);
        assert_eq!(
            second, first,
            "a later record never lands before an earlier one"
        );

        let third = age.stamp(Some(WallTime::from_unix_millis(now)), received);
        assert_eq!(third, received, "and a record written now is happening now");
    }

    #[test]
    fn a_record_with_no_timestamp_keeps_the_receipt_clock() {
        // Every sidecar record is timestamp-free, and there is nothing better to
        // date one by than the moment it was read.
        let mut age = Aging::start();
        let received = Instant::now();
        assert_eq!(age.stamp(None, received), received);
    }

    #[test]
    fn tailing_follows_an_append_without_duplicating_or_dropping_a_record() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("p");
        let main = project.join("sess.jsonl");
        let text = read_fixture("main-session-mixed.jsonl");
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let split = lines.len() / 2;

        write(&main, &format!("{}\n", lines[..split].join("\n")));
        let mut tailer = SessionTailer::open(&project.join("sess"), StartAt::Beginning).unwrap();
        let (first, _) = poll_count(&mut tailer);
        assert_eq!(first, split);
        assert_eq!(poll_count(&mut tailer).0, 0, "a quiet file yields nothing");

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&main)
            .unwrap();
        writeln!(f, "{}", lines[split..].join("\n")).unwrap();
        drop(f);

        let (second, events) = poll_count(&mut tailer);
        assert_eq!(first + second, lines.len(), "every record, exactly once");
        assert_eq!(tailer.stats().ok, lines.len() as u64);
        // Byte offsets strictly increase, which is the ordering key (ADR-0014).
        let offsets: Vec<u64> = events
            .iter()
            .filter_map(|e| match &e.payload {
                Payload::Transcript(t) => Some(t.byte_offset),
                _ => None,
            })
            .collect();
        assert!(offsets.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(
            tailer.offsets().get(&main).copied(),
            Some(std::fs::metadata(&main).unwrap().len())
        );
    }

    #[test]
    fn a_half_written_final_line_is_waited_for_never_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("p");
        let main = project.join("sess.jsonl");
        write(
            &main,
            "{\"type\":\"mode\",\"sessionId\":\"s\"}\n{\"type\":\"us",
        );

        let mut tailer = SessionTailer::open(&project.join("sess"), StartAt::Beginning).unwrap();
        assert_eq!(poll_count(&mut tailer).0, 1, "only the complete record");
        assert_eq!(tailer.stats().bad_json, 0, "a torn tail is not corruption");

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&main)
            .unwrap();
        writeln!(
            f,
            "er\",\"sessionId\":\"s\",\"message\":{{\"role\":\"user\",\"content\":\"x\"}}}}"
        )
        .unwrap();
        drop(f);
        assert_eq!(poll_count(&mut tailer).0, 1, "completed and then parsed");
        assert_eq!(tailer.stats().ok, 2);
        assert_eq!(tailer.stats().bad_json, 0);
    }

    #[test]
    fn tailing_survives_truncation_and_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("p");
        let main = project.join("sess.jsonl");
        write(
            &main,
            "{\"type\":\"mode\",\"sessionId\":\"s\",\"mode\":\"a\"}\n\
             {\"type\":\"mode\",\"sessionId\":\"s\",\"mode\":\"b\"}\n",
        );
        let mut tailer = SessionTailer::open(&project.join("sess"), StartAt::Beginning).unwrap();
        assert_eq!(poll_count(&mut tailer).0, 2);

        // Rotation: the file is replaced by a shorter one.
        write(
            &main,
            "{\"type\":\"mode\",\"sessionId\":\"s\",\"mode\":\"c\"}\n",
        );
        let (n, events) = poll_count(&mut tailer);
        assert_eq!(n, 1, "the replacement is read from 0");
        assert_eq!(tailer.stats().truncations, 1);
        assert!(
            events.iter().any(|e| matches!(
                &e.payload,
                Payload::Control(ControlEvent::SchemaDrift { .. })
            )),
            "a truncation is visible, not silent"
        );
        let modes: Vec<String> = events
            .iter()
            .filter_map(|e| match &e.payload {
                Payload::Transcript(t) => t
                    .record
                    .get("mode")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                _ => None,
            })
            .collect();
        assert_eq!(modes, vec!["c".to_owned()]);
    }

    #[test]
    fn a_subagent_file_that_appears_at_runtime_is_picked_up() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("p");
        let sid = "sess";
        write(
            &project.join("sess.jsonl"),
            "{\"type\":\"mode\",\"sessionId\":\"s\"}\n",
        );
        let mut tailer = SessionTailer::open(&project.join(sid), StartAt::Beginning).unwrap();
        assert_eq!(poll_count(&mut tailer).0, 1);
        assert_eq!(tailer.files().count(), 1);

        // The agent spawns: a whole new file appears under a directory that did
        // not exist when the tailer opened.
        write(
            &project
                .join(sid)
                .join("subagents/workflows/wf_r1/agent-a7.jsonl"),
            "{\"type\":\"assistant\",\"sessionId\":\"s\",\"agentId\":\"a7\",\
             \"isSidechain\":true,\"uuid\":\"u\",\"parentUuid\":null,\
             \"message\":{\"role\":\"assistant\",\"content\":[]}}\n",
        );
        let (n, events) = poll_count(&mut tailer);
        assert_eq!(n, 1, "the tailer watches the DIRECTORY, not just the file");
        assert_eq!(tailer.files().count(), 2);
        let worker = events
            .iter()
            .find_map(|e| e.meta.worker.as_ref())
            .expect("attributed");
        assert_eq!(worker.as_str(), "a7");
    }

    #[test]
    fn a_deleted_transcript_drops_out_instead_of_killing_the_channel() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("p");
        let main = project.join("sess.jsonl");
        write(&main, "{\"type\":\"mode\",\"sessionId\":\"s\"}\n");
        let mut tailer = SessionTailer::open(&project.join("sess"), StartAt::Beginning).unwrap();
        assert_eq!(poll_count(&mut tailer).0, 1);
        std::fs::remove_file(&main).unwrap();
        assert_eq!(poll_count(&mut tailer).0, 0);
        assert_eq!(tailer.files().count(), 0, "dropped, not fatal");
    }

    #[test]
    fn resume_reads_only_what_arrived_while_polis_was_down() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("p");
        let main = project.join("sess.jsonl");
        write(&main, "{\"type\":\"mode\",\"sessionId\":\"s\"}\n");
        let mut tailer = SessionTailer::open(&project.join("sess"), StartAt::Beginning).unwrap();
        assert_eq!(poll_count(&mut tailer).0, 1);
        let offsets = tailer.offsets();
        drop(tailer);

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&main)
            .unwrap();
        writeln!(
            f,
            "{{\"type\":\"ai-title\",\"sessionId\":\"s\",\"aiTitle\":\"t\"}}"
        )
        .unwrap();
        drop(f);

        let mut resumed = SessionTailer::resume(&project.join("sess"), &offsets).unwrap();
        assert_eq!(poll_count(&mut resumed).0, 1, "only the new record");
        // And starting at the end skips everything.
        let mut fresh = SessionTailer::open(&project.join("sess"), StartAt::End).unwrap();
        assert_eq!(poll_count(&mut fresh).0, 0);
    }

    #[test]
    fn the_projects_tailer_discovers_sessions_that_appear_later() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tailer = ProjectsTailer::open(tmp.path(), StartAt::Beginning).unwrap();
        assert!(tailer.is_empty());
        assert_eq!(tailer.initial_start(), StartAt::Beginning);

        write(
            &tmp.path().join("C--coding-x").join("aaa.jsonl"),
            "{\"type\":\"mode\",\"sessionId\":\"s\"}\n",
        );
        let mut out = Vec::new();
        assert_eq!(tailer.poll(&mut out), 1);
        assert_eq!(tailer.len(), 1);
        assert_eq!(tailer.stats().ok, 1);
    }

    // -- offline replay (PRD §15 M2) --------------------------------------

    #[test]
    fn replay_reads_the_whole_forest_in_byte_order() {
        let tmp = tempfile::tempdir().unwrap();
        let (session_dir, _) = build_session(tmp.path());
        let replay = read_session(&session_dir, &PathMapper::default()).unwrap();

        let main_lines = read_fixture("main-session-with-subagent.jsonl")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        let sub_lines = read_fixture("subagent-a96cf8a57447af436.jsonl")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        assert_eq!(replay.len(), main_lines + sub_lines);
        assert_eq!(replay.stats.bad_json, 0);
        assert_eq!(replay.stats.bad_shape, 0);
        assert!(replay.header.is_supported());
        assert_eq!(replay.files.len(), 2);

        // The main transcript comes first, so the `Agent` tool_use is seen
        // before the subagent's own records.
        let sources: Vec<bool> = replay
            .events
            .iter()
            .filter_map(|e| match e.payload() {
                Payload::Transcript(t) => Some(matches!(t.source, TranscriptSource::Main)),
                _ => None,
            })
            .collect();
        let first_sub = sources.iter().position(|m| !m).unwrap();
        assert!(sources[..first_sub].iter().all(|m| *m));
        assert!(sources[first_sub..].iter().all(|m| !*m));
    }

    #[test]
    fn a_recording_sorts_the_same_by_wall_clock_and_by_offset() {
        // Deliberately backwards timestamps — 20% of real files have them, and
        // one observed jump was 60 seconds (ADR-0014).
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("p").join("sess.jsonl");
        write(
            &main,
            "{\"type\":\"user\",\"sessionId\":\"s\",\"timestamp\":\"2026-08-24T21:51:09.661Z\",\
             \"message\":{\"role\":\"user\",\"content\":\"a\"}}\n\
             {\"type\":\"user\",\"sessionId\":\"s\",\"timestamp\":\"2026-08-24T21:50:09.120Z\",\
             \"message\":{\"role\":\"user\",\"content\":\"b\"}}\n\
             {\"type\":\"mode\",\"sessionId\":\"s\",\"mode\":\"normal\"}\n\
             {\"type\":\"user\",\"sessionId\":\"s\",\"timestamp\":\"2026-08-24T21:52:00.000Z\",\
             \"message\":{\"role\":\"user\",\"content\":\"c\"}}\n",
        );
        let replay = read_transcript(&main, &PathMapper::default()).unwrap();
        assert_eq!(replay.len(), 4);

        let walls: Vec<i64> = replay.events.iter().map(|e| e.wall.unix_millis()).collect();
        let offsets: Vec<u64> = replay
            .events
            .iter()
            .map(|e| e.monotonic_offset_ms)
            .collect();
        assert!(walls.windows(2).all(|w| w[0] <= w[1]), "{walls:?}");
        assert!(offsets.windows(2).all(|w| w[0] <= w[1]), "{offsets:?}");
        // The origin is the EARLIEST timestamp anywhere in the file — here the
        // *second* record, 60.541 s before the first — so no offset is negative.
        assert_eq!(
            replay.header.wall_origin,
            parse_timestamp("2026-08-24T21:50:09.120Z").unwrap()
        );
        assert_eq!(offsets[0], 60_541);
        assert_eq!(
            offsets[1], 60_541,
            "the backwards step is clamped, not reordered and not negative"
        );
        assert_eq!(
            offsets[2], 60_541,
            "a timestamp-free sidecar inherits the running clock"
        );
        assert_eq!(offsets[3], 110_880);
        // The verbatim, un-monotonised timestamp survives inside the record.
        let Payload::Transcript(second) = replay.events[1].payload() else {
            panic!()
        };
        assert_eq!(
            second.record.get("timestamp").and_then(Value::as_str),
            Some("2026-08-24T21:50:09.120Z")
        );
        // Byte order is preserved regardless of what the clocks said.
        let contents: Vec<String> = replay
            .events
            .iter()
            .filter_map(|e| match e.payload() {
                Payload::Transcript(t) => t
                    .record
                    .pointer("/message/content")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                _ => None,
            })
            .collect();
        assert_eq!(contents, vec!["a", "b", "c"]);
    }

    #[test]
    fn a_recording_round_trips_and_is_byte_identical_across_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let (session_dir, _) = build_session(tmp.path());
        let mapper = PathMapper::default();

        let mut first = Vec::new();
        read_session(&session_dir, &mapper)
            .unwrap()
            .write_jsonl(&mut first)
            .unwrap();
        let mut second = Vec::new();
        read_session(&session_dir, &mapper)
            .unwrap()
            .write_jsonl(&mut second)
            .unwrap();
        assert_eq!(first, second, "PRD §7.4 — no wall clock, no hash order");

        let text = String::from_utf8(first).unwrap();
        let mut lines = text.lines();
        let header: RecordingHeader = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert!(header.is_supported());
        assert_eq!(header.format, RECORDING_FORMAT);
        let mut n = 0;
        for line in lines {
            let event = RecordedEvent::from_json_line(line).expect("round trip");
            assert_eq!(event.channel(), Channel::Transcript);
            n += 1;
        }
        assert!(n > 30);
    }

    #[test]
    fn replayed_events_keep_their_inter_arrival_gaps() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("p").join("sess.jsonl");
        write(
            &main,
            "{\"type\":\"user\",\"sessionId\":\"s\",\"timestamp\":\"2026-08-24T10:00:00.000Z\",\
             \"message\":{\"role\":\"user\",\"content\":\"a\"}}\n\
             {\"type\":\"user\",\"sessionId\":\"s\",\"timestamp\":\"2026-08-24T10:00:00.860Z\",\
             \"message\":{\"role\":\"user\",\"content\":\"b\"}}\n",
        );
        let replay = read_transcript(&main, &PathMapper::default()).unwrap();
        assert_eq!(replay.events[1].monotonic_offset_ms, 860);
        let live = replay.into_live(ReplayClock::start_now());
        let gap = live[1].meta.observed.duration_since(live[0].meta.observed);
        assert_eq!(gap, Duration::from_millis(860));
    }

    #[test]
    fn replay_of_a_single_fixture_file_needs_no_session_layout() {
        let mapper = PathMapper::default();
        let replay = read_transcript(&fixture("main-session-mixed.jsonl"), &mapper).unwrap();
        assert_eq!(replay.len(), 94);
        assert_eq!(replay.stats.drift(), 0);
        assert!(replay
            .events
            .iter()
            .all(|e| e.channel() == Channel::Transcript));
    }

    // -- record hygiene ---------------------------------------------------

    #[test]
    fn account_identifiers_never_reach_the_bus() {
        let line = r#"{"type":"bridge-session","sessionId":"s","bridgeSessionId":"cse_1",
            "lastSequenceNum":0,"ownerAccountUuid":"11111111-1111-1111-1111-111111111111",
            "ownerOrganizationUuid":"22222222-2222-2222-2222-222222222222"}"#;
        let event =
            transcript_line(line, &TranscriptSource::Main, 0, &PathMapper::default()).unwrap();
        let Payload::Transcript(t) = &event.payload else {
            panic!()
        };
        assert!(t.record.get("ownerAccountUuid").is_none(), "ADR-0005");
        assert!(t.record.get("ownerOrganizationUuid").is_none());
        assert_eq!(
            t.record.get("bridgeSessionId").and_then(Value::as_str),
            Some("cse_1"),
            "only the identity document is dropped, not the record"
        );
    }

    #[test]
    fn very_long_strings_are_elided_with_their_counts_kept() {
        let long = format!("{}\n{}", "x".repeat(4000), "y".repeat(4000));
        let mut value = serde_json::json!({ "content": long, "n": 1 });
        assert_eq!(elide_long_strings(&mut value, MAX_RECORD_STRING), 1);
        let out = value["content"].as_str().unwrap();
        assert!(out.len() < 4000 + 200);
        assert!(out.ends_with("chars, 2 lines elided]"), "{out}");
        // Opting out is possible and exact.
        let mut untouched = serde_json::json!({ "content": "z".repeat(9000) });
        assert_eq!(elide_long_strings(&mut untouched, usize::MAX), 0);
        assert_eq!(untouched["content"].as_str().unwrap().len(), 9000);
    }

    // -- timestamps -------------------------------------------------------

    #[test]
    fn timestamps_parse_in_the_one_shape_the_corpus_uses() {
        let t = parse_timestamp("2026-08-24T22:43:22.764Z").unwrap();
        assert_eq!(t.unix_millis(), 1_787_611_402_764);
        assert_eq!(
            parse_timestamp("1970-01-01T00:00:00.000Z").unwrap(),
            WallTime::UNIX_EPOCH
        );
        // A leap day, because `days_from_civil` is written out rather than
        // pulled from a calendar crate.
        assert_eq!(
            parse_timestamp("2024-02-29T00:00:00.000Z")
                .unwrap()
                .unix_seconds(),
            1_709_164_800
        );
        assert_eq!(
            parse_timestamp("2026-08-24T22:43:22+02:00")
                .unwrap()
                .unix_seconds(),
            parse_timestamp("2026-08-24T20:43:22Z")
                .unwrap()
                .unix_seconds()
        );
        for bad in [
            "",
            "2026-08-24",
            "not a timestamp at all!!",
            "2026-13-01T00:00:00.000Z",
            "20260824T224322Z",
            "2026-08-24T99:99:99.999Z",
            "999999999999-08-24T00:00:00Z",
        ] {
            assert!(parse_timestamp(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn every_fixture_timestamp_parses() {
        for name in [
            "main-session-mixed.jsonl",
            "main-session-with-subagent.jsonl",
            "subagent-a96cf8a57447af436.jsonl",
        ] {
            let text = read_fixture(name);
            let (records, _) = parse_all(&text);
            let mut seen = 0;
            for r in &records {
                if let Some(ts) = r.timestamp() {
                    assert!(parse_timestamp(ts).is_some(), "{name}: {ts}");
                    seen += 1;
                }
            }
            assert!(seen > 0, "{name} carries timestamps");
        }
    }

    // -- Windows long paths (ADR-0034) -------------------------------------

    /// A `cwd` deep enough to trip the 200-character cap yields a 282-character
    /// transcript path on a machine with `LongPathsEnabled = 0`. Rust's
    /// `std::fs` copes; Python's `io.open` does not. Opportunistic: this only
    /// runs where such a directory exists.
    #[test]
    fn a_transcript_past_max_path_is_still_readable() {
        let Some(home) = std::env::var_os("USERPROFILE") else {
            return;
        };
        let projects = Path::new(&home).join(".claude").join("projects");
        let Ok(dirs) = discover_project_dirs(&projects) else {
            return;
        };
        for dir in dirs {
            let Ok(files) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in files.filter_map(Result::ok) {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "jsonl") && path.as_os_str().len() > 260 {
                    let bytes =
                        std::fs::read(&path).expect("std::fs handles >MAX_PATH via verbatim paths");
                    assert!(!bytes.is_empty());
                    let replay = read_transcript(&path, &PathMapper::default()).unwrap();
                    assert!(replay.stats.total() > 0);
                    return;
                }
            }
        }
    }

    /// The strongest claim available: parse **every** transcript on this
    /// machine and assert the model holds over all of it, not just over four
    /// checked-in fixtures.
    ///
    /// `#[ignore]` because it depends on a local `~/.claude/projects` that CI
    /// does not have. Run it after every Claude Code upgrade — a new `version`
    /// string in the output is the leading indicator that the rest of these
    /// counters are about to move:
    ///
    /// ```text
    /// cargo test -p polis-ingest --lib -- --ignored --nocapture corpus
    /// ```
    #[test]
    #[ignore = "reads the local ~/.claude/projects corpus; not present in CI"]
    fn the_whole_local_corpus_parses() {
        let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
        else {
            return;
        };
        let projects = Path::new(&home).join(".claude").join("projects");
        let mut stats = ParseStats::default();
        let mut files = 0usize;
        let mut long_paths = 0usize;
        let mut sources: BTreeMap<&'static str, usize> = BTreeMap::new();

        let mut all = Vec::new();
        for project in discover_project_dirs(&projects).expect("a local corpus") {
            walk_jsonl(&project, 0, &mut all).unwrap();
        }
        for path in &all {
            files += 1;
            if path.as_os_str().len() > 260 {
                long_paths += 1;
            }
            let label = match transcript_source(path) {
                Some(TranscriptSource::Main) => "main",
                Some(TranscriptSource::Subagent { .. }) => "subagent",
                Some(TranscriptSource::WorkflowJournal { .. }) => "journal",
                None => "other",
            };
            *sources.entry(label).or_default() += 1;
            let bytes = std::fs::read(path).expect("std::fs handles >MAX_PATH");
            for (_, line) in lines_with_offsets(&bytes) {
                let _ = parse_line(&String::from_utf8_lossy(line), &mut stats);
            }
        }

        println!("files={files} long_paths={long_paths} sources={sources:?}");
        println!(
            "ok={} blank={} bad_json={} bad_shape={} unknown_type={}",
            stats.ok, stats.blank, stats.bad_json, stats.bad_shape, stats.unknown_type
        );
        println!("versions={:?}", stats.versions_seen);
        println!("unknown_types={:?}", stats.unknown_types_seen);
        assert!(files > 0, "no transcripts found");
        assert_eq!(
            stats.bad_json, 0,
            "a real transcript failed to parse as JSON"
        );
        assert_eq!(stats.bad_shape, 0, "the model does not fit the real corpus");
        assert_eq!(
            stats.unknown_type, 0,
            "a record type this build does not model: {:?}",
            stats.unknown_types_seen
        );
    }

    // -- properties -------------------------------------------------------

    proptest::proptest! {
        /// PRD §16: "the parser must degrade, never panic." The property is
        /// total over every `&str`, which is exactly what the fuzz target
        /// asserts over every `&[u8]`.
        #[test]
        fn parse_line_never_panics_and_always_counts(line in ".{0,400}") {
            let mut stats = ParseStats::default();
            let _ = parse_line(&line, &mut stats);
            proptest::prop_assert_eq!(stats.total(), 1);
        }

        /// The project key is filename-safe for any input, and never panics —
        /// including on the `i32::MIN` path that `.abs()` would blow up on.
        #[test]
        fn project_key_is_always_filename_safe(cwd in ".{0,600}") {
            let key = project_key(&cwd);
            proptest::prop_assert!(
                key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            );
            proptest::prop_assert!(key.len() <= 214);
        }

        /// Elision never produces invalid UTF-8 and never grows a short string.
        #[test]
        fn elision_is_safe_on_any_string(s in ".{0,600}") {
            let mut v = Value::String(s.clone());
            let n = elide_long_strings(&mut v, 10);
            let out = v.as_str().unwrap();
            proptest::prop_assert_eq!(n > 0, s.chars().count() > 10);
            proptest::prop_assert!(out.starts_with(
                &s.chars().take(10).collect::<String>()
            ));
        }
    }
}
