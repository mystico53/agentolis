//! The normalised [`Event`] the four ingest channels feed into (PRD §4.5).
//!
//! > All four channels normalize into one internal `Event` enum and push into a
//! > bounded `crossbeam` channel (capacity 65536) consumed by the world-state
//! > thread.
//!
//! The variant names below are **the real wire names**, taken from the recon
//! documents rather than invented, so that a reader with the Claude Code docs
//! open can match them one-to-one:
//!
//! | Channel | Variants derived from |
//! |---|---|
//! | A — OpenTelemetry | `docs/verified/otel-schema.md` event and metric inventory |
//! | B — Hooks | `docs/verified/hooks-schema.md` §1, via [`crate::EventKind`] |
//! | C — Filesystem | `notify`'s event kinds, reduced to what PRD §4.3 uses |
//! | D — JSONL | `docs/verified/jsonl-schema.md` §2 type histogram (19 kinds) |
//!
//! # Ordering
//!
//! Order by [`EventMeta::observed`] (the moment Polis received the datagram or
//! the batch), never by a timestamp inside a payload:
//!
//! * Transcript timestamps are **not monotonic** — 20% of files contain a
//!   backwards step and one observed jump was 60 seconds. They are display-only;
//!   record order is file byte order.
//! * OTLP log records carry `event.sequence`, monotonic from 0 per session; use
//!   it for total order *within* a session and as a free loss detector, which is
//!   what [`ControlEvent::SequenceGap`] reports.
//! * OTLP spans arrive **before their parents** — batches are not topologically
//!   ordered.
//!
//! This directly corrects PRD §4.3's "±2s window": the window must be anchored
//! on the hook or OTel clock, which is stamped at the moment of the call, never
//! on a transcript timestamp (ADR-0014).

use std::fmt;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::ids::{AgentType, PromptId, SessionId, ThreadId, ToolUseId, WorkerId, WorktreeId};
use crate::kind::EventKind;
use crate::path::LogicalPath;
use crate::tool::{Outcome, ToolKind};

/// Which ingest channel produced an event (PRD §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Channel {
    /// A — OpenTelemetry. Bulk events, in-process batched export, zero spawn cost.
    Otel,
    /// B — Hooks. Rare, latency-critical, one process spawn each.
    Hook,
    /// C — Filesystem watch. Ground truth about disk, blind about attribution.
    Fs,
    /// D — JSONL transcript tailing. Reconciliation, cold start, replay.
    Transcript,
    /// Polis's own health signals. Not a Claude Code channel.
    Control,
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Otel => "otel",
            Self::Hook => "hook",
            Self::Fs => "fs",
            Self::Transcript => "transcript",
            Self::Control => "control",
        })
    }
}

/// Everything an event carries independently of its channel.
///
/// Every identity field is `Option` on purpose: absence is routine and often
/// *meaningful*. A `None` [`worker`](Self::worker) means the main agent, not a
/// parse failure.
///
/// # `observed` does not serialize
///
/// [`Instant`] has no wire representation and deliberately gains none here: the
/// live path is the only place it means anything, because it is the only clock
/// that cannot step backwards (ADR-0014). The recorded path carries time
/// separately, as [`crate::record::RecordedEvent`]'s wall clock plus monotonic
/// offset, and [`crate::record::ReplayClock`] puts a coherent `Instant` back on
/// the way in. Serializing an [`Event`] by itself therefore loses its timing and
/// deserializing one stamps it `Instant::now()`; go through `RecordedEvent`
/// (ADR-0049).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMeta {
    /// When Polis received it. The only clock safe to window on (see the module
    /// docs). Not serialized — see the type docs.
    #[serde(skip, default = "Instant::now")]
    pub observed: Instant,
    /// Which channel produced it.
    pub channel: Channel,
    /// The session. Present on essentially everything, including OTLP metric
    /// data points, which is why this and not `prompt.id` is the session key.
    pub session: Option<SessionId>,
    /// The thread the event belongs to (PRD §3). Derived as
    /// `ThreadId::of_session(session)` — **not** from `agent_id`, because a
    /// subagent's records carry the parent session's id.
    pub thread: Option<ThreadId>,
    /// The worker, when the event came from inside a subagent. `None` means the
    /// main agent.
    pub worker: Option<WorkerId>,
    /// The worker's *type* (`Explore`, `general-purpose`, …). Never an identity.
    pub agent_type: Option<AgentType>,
    /// The turn key. Absent on all startup traffic and never on metrics.
    pub prompt: Option<PromptId>,
    /// `event.sequence` from the OTel channel: monotonic per session from 0.
    /// A gap means records were dropped.
    pub sequence: Option<u64>,
}

impl EventMeta {
    /// A bare envelope stamped now, for a channel with no identity yet.
    pub fn now(channel: Channel) -> Self {
        Self {
            observed: Instant::now(),
            channel,
            session: None,
            thread: None,
            worker: None,
            agent_type: None,
            prompt: None,
            sequence: None,
        }
    }

    /// Attaches a session and derives the thread from it.
    #[must_use]
    pub fn with_session(mut self, session: SessionId) -> Self {
        self.thread = Some(ThreadId::of_session(session.clone()));
        self.session = Some(session);
        self
    }

    /// True when the event came from inside a subagent.
    ///
    /// Keyed on [`worker`](Self::worker) presence **only**: a main agent started
    /// with `claude --agent foo` also carries an
    /// [`agent_type`](Self::agent_type).
    pub fn is_worker(&self) -> bool {
        self.worker.is_some()
    }
}

/// One normalised event on the bus.
///
/// Serializable so [`crate::record::RecordedEvent`] can wrap it, but note that
/// [`EventMeta::observed`] is skipped: an `Event` on its own carries no time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Channel-independent envelope.
    pub meta: EventMeta,
    /// The channel-specific body.
    pub payload: Payload,
}

impl Event {
    /// Pairs an envelope with a body.
    pub fn new(meta: EventMeta, payload: Payload) -> Self {
        Self { meta, payload }
    }

    /// A control event stamped now.
    pub fn control(control: ControlEvent) -> Self {
        Self::new(EventMeta::now(Channel::Control), Payload::Control(control))
    }
}

/// The channel-specific body of an [`Event`].
///
/// Boxed variants keep the enum small: the bus holds 65 536 of these
/// (PRD §4.5), so an unboxed 600-byte variant would cost tens of megabytes of
/// resident memory for the queue alone.
///
/// `#[non_exhaustive]`: a fifth Claude Code channel, or a second class of Polis
/// health signal, must not break every `match` in `polis-world` (ADR-0048).
///
/// The variant names are the recorded wire format (ADR-0049): serde's default
/// external tagging writes `{"Hook": {...}}`, so renaming a variant invalidates
/// every recording on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Payload {
    /// Channel A, logs and traces.
    Otel(Box<OtelEvent>),
    /// Channel A, metrics. Kept separate because metrics carry no `prompt.id`
    /// and obey a different accumulation rule.
    Metric(Box<OtelMetric>),
    /// Channel B.
    Hook(Box<HookEvent>),
    /// Channel C.
    Fs(FsEvent),
    /// Channel D.
    Transcript(Box<TranscriptEvent>),
    /// Polis's own health.
    Control(ControlEvent),
}

// ---------------------------------------------------------------------------
// Channel A — OpenTelemetry
// ---------------------------------------------------------------------------

/// A Claude Code OpenTelemetry log record or span (PRD §4.1).
///
/// The event name lives in the `LogRecord` **body** (`claude_code.tool_result`),
/// with an un-prefixed short name in the `event.name` attribute. The OTLP 1.7
/// `LogRecord.event_name` field is empty on every record, and `severity_number`
/// is `UNSPECIFIED` on every record — so neither is usable for dispatch.
///
/// `#[non_exhaustive]`: this enum tracks a **beta** schema that
/// `docs/verified/otel-schema.md` expects to drift, and [`OTEL_EVENT_NAMES`] is
/// 26 entries long. Every new `claude_code.*` event would otherwise be a
/// workspace-wide compile break instead of the drift signal it is (ADR-0048).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum OtelEvent {
    /// `user_prompt` — the start of a turn; mints the [`PromptId`].
    UserPrompt {
        /// Prompt length in characters. Arrives as a *string* on the wire.
        length: Option<u64>,
    },
    /// `assistant_response`.
    AssistantResponse,
    /// `api_request`. Carries per-request token counts at ~1 s cadence, which is
    /// the right source for live token display — the metrics channel lags by up
    /// to `OTEL_METRIC_EXPORT_INTERVAL`.
    ApiRequest {
        /// Model id **without** the `[1m]` context suffix the metrics channel
        /// adds. Joining on model across signals needs normalisation.
        model: Option<String>,
        /// `sdk`, `generate_session_title`, or `agent:builtin:<type>` — a
        /// different vocabulary from the metric-side `query_source`.
        query_source: Option<String>,
        /// Milliseconds. Arrives as an int on some events and a double on others.
        duration_ms: Option<f64>,
    },
    /// `api_error`.
    ApiError {
        /// Error class, when the record carries one.
        error_type: Option<String>,
    },
    /// `api_refusal`.
    ApiRefusal,
    /// `api_retries_exhausted`.
    ApiRetriesExhausted,
    /// `tool_decision` — the permission audit record.
    ///
    /// Emitted *after* the fact and carries **no file path** for `Read`, `Write`
    /// or `Edit`, so it cannot drive anything that must act before a write lands.
    /// It does capture rejections, which `tool_result` never does.
    ToolDecision {
        /// The tool the decision was about.
        tool: ToolKind,
        /// Joins to `tool_result` and to the hook payload.
        tool_use_id: Option<ToolUseId>,
        /// `accept` / `reject`, verbatim.
        decision: Option<String>,
        /// Where the decision came from.
        source: Option<String>,
    },
    /// `tool_result` — the workhorse of Channel A.
    ToolResult(Box<ToolCall>),
    /// `subagent_completed`. `total_tool_uses` is a whole-run total;
    /// `total_tokens` covers only the subagent's **final** API request, so it is
    /// not a run total and must not be summed as one.
    SubagentCompleted {
        /// Total tool calls over the subagent's whole run.
        total_tool_uses: Option<u64>,
        /// The subagent's type.
        agent_type: Option<AgentType>,
    },
    /// `permission_mode_changed`. Fires before the first prompt, so it has no
    /// [`PromptId`].
    PermissionModeChanged {
        /// `default` / `plan` / `acceptEdits` / `auto` / `dontAsk` /
        /// `bypassPermissions`.
        mode: Option<String>,
    },
    /// `mcp_server_connection`.
    McpServerConnection {
        /// Server name.
        server: Option<String>,
    },
    /// `plugin_loaded`. Startup traffic; no `prompt.id`.
    PluginLoaded {
        /// Plugin name.
        name: Option<String>,
    },
    /// `plugin_installed`.
    PluginInstalled {
        /// Plugin name.
        name: Option<String>,
    },
    /// `skill_activated`.
    SkillActivated {
        /// Skill name.
        name: Option<String>,
    },
    /// `at_mention` — a file the operator pinned with `@`.
    ///
    /// Load-bearing for PRD §6.1: an `@`-referenced file is added to context with
    /// **no tool call at all**, so it is invisible to territory inference unless
    /// this event is consumed.
    AtMention,
    /// `compaction`.
    Compaction,
    /// `auth`.
    Auth,
    /// `internal_error`.
    InternalError,
    /// `hook_registered`.
    HookRegistered,
    /// `hook_execution_start`.
    HookExecutionStart,
    /// `hook_execution_complete`.
    HookExecutionComplete,
    /// `hook_plugin_metrics`.
    HookPluginMetrics,
    /// `retention_sweep`.
    RetentionSweep,
    /// `feedback_survey`.
    FeedbackSurvey,
    /// A `claude_code.tool` **span** from the beta traces channel.
    ///
    /// This is the only place a subagent's tool call is distinguishable from the
    /// main agent's: `agent_id` is present on the span for a subagent and absent
    /// for the main agent, and the span also carries a clean top-level
    /// `file_path` that needs no JSON parsing. Polis's thread model depends on
    /// this beta signal; see [`ControlEvent::ChannelDegraded`] for the fallback.
    ToolSpan {
        /// The call this span describes.
        call: Box<ToolCall>,
        /// 16 hex characters. Separates concurrent subagents even when the
        /// `agent_id` attribute is absent.
        span_id: Option<String>,
        /// The parent span. Batches are not topologically ordered, so parentage
        /// must be resolved lazily.
        parent_span_id: Option<String>,
    },
    /// A `claude_code.interaction` **span** — the trace root, one per user
    /// prompt (`docs/verified/otel-schema.md` §6.1).
    ///
    /// This is the only whole-turn boundary either signal carries.
    /// **`trace_id` is one-per-interaction, not one-per-session**: an auxiliary
    /// request such as the session-title generation arrives under its own
    /// `trace_id` with no parent, so grouping spans by `trace_id` does not group
    /// them by session. Group by `session.id`, which is on every span.
    InteractionSpan {
        /// 16 hex characters. The parent of this turn's tool and LLM spans.
        span_id: Option<String>,
        /// `interaction.sequence` — orders turns within a session.
        sequence: Option<u64>,
        /// `interaction.duration_ms`.
        duration_ms: Option<f64>,
        /// `user_prompt_length`. The prompt text itself is `<REDACTED>` by
        /// default and Polis never un-redacts it (ADR-0005).
        prompt_length: Option<u64>,
    },
    /// A `claude_code.llm_request` **span** — one API call.
    ///
    /// Carries `agent_id`, so together with [`OtelEvent::ToolSpan`] it is the
    /// second place a subagent's work is distinguishable from the main agent's.
    /// A subagent's `llm_request` spans nest under the spawning `Agent` tool's
    /// `tool.execution` span.
    LlmRequestSpan {
        /// 16 hex characters.
        span_id: Option<String>,
        /// The parent span, absent for a standalone request.
        parent_span_id: Option<String>,
        /// `model`, without the `[1m]` suffix the metric channel adds.
        model: Option<String>,
        /// `llm_request.context`: `tool` for a call made inside a tool,
        /// `standalone` for an auxiliary request that belongs to no
        /// interaction. Both observed.
        context: Option<String>,
        /// `duration_ms`.
        duration_ms: Option<f64>,
        /// From `success`, which arrives as a string on this channel.
        outcome: Outcome,
    },
    /// A `claude_code.tool.execution` **span** — the execution half of a tool
    /// call, excluding any permission wait.
    ///
    /// Distinct from [`OtelEvent::ToolSpan`]'s `duration_ms`, which includes the
    /// wait. For the `Agent` tool this span is the parent of the whole
    /// subagent's span subtree.
    ToolExecutionSpan {
        /// Joins to the enclosing [`OtelEvent::ToolSpan`] and to everything else.
        tool_use_id: Option<ToolUseId>,
        /// The enclosing `claude_code.tool` span.
        parent_span_id: Option<String>,
        /// `duration_ms`.
        duration_ms: Option<f64>,
        /// From `success`.
        outcome: Outcome,
    },
    /// A `claude_code.tool.blocked_on_user` **span** — how long a tool call
    /// waited on a human.
    ///
    /// Directly measures PRD §11.2 attention state (a): a thread parked on a
    /// permission prompt. Its `decision` and `source` attributes were both
    /// observed as the literal string `"unknown"` and must not be trusted — use
    /// [`OtelEvent::ToolDecision`] for the decision itself.
    ToolBlockedOnUserSpan {
        /// Joins to the enclosing [`OtelEvent::ToolSpan`].
        tool_use_id: Option<ToolUseId>,
        /// The enclosing `claude_code.tool` span.
        parent_span_id: Option<String>,
        /// How long the human took. The only channel that measures this.
        duration_ms: Option<f64>,
    },
    /// Any event or span name Polis does not model. Logged at debug and
    /// dropped, never fatal (PRD §4.1).
    ///
    /// `claude_code.hook` lands here on purpose: capturing it needs *detailed*
    /// beta tracing, which redirects logs and traces to a separate endpoint and
    /// would hijack Polis's own export destination.
    Unknown {
        /// The `event.name` attribute or span name, verbatim.
        name: String,
    },
}

/// A tool call, however it was observed.
///
/// Assembled from `tool_result`, the `claude_code.tool` span, a `PostToolUse`
/// hook, or a transcript `tool_use` block. `tool_use_id` is the join key across
/// all of them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Which tool.
    pub tool: ToolKind,
    /// `toolu_…`, when known.
    pub tool_use_id: Option<ToolUseId>,
    /// Paths the call touched, already normalised (PRD §7.6).
    ///
    /// Preferred source is the span's top-level `file_path`; the fallback is
    /// `file_path` inside the `tool_input` JSON *string*, which must be
    /// JSON-decoded rather than sliced. `tool_input` values are truncated at 128
    /// characters with a `…[N chars]` marker — `file_path` is unaffected because
    /// it is the first and shortest key, but nothing else here may be assumed
    /// complete.
    pub paths: Vec<(WorktreeId, LogicalPath)>,
    /// How it went (PRD §10.2). `success` arrives as a string, not a bool.
    pub outcome: Outcome,
    /// Wall time, when reported.
    pub duration_ms: Option<f64>,
}

/// One of the eight Claude Code counters (PRD §4.1).
///
/// All eight are **monotonic sums**, and every one observed on this machine used
/// `AGGREGATION_TEMPORALITY_DELTA`: each export is the increment since the last,
/// not a running total. Polis must **accumulate**. Reading a delta stream as
/// cumulative is a silent failure — every counter simply collapses to "the last
/// export interval" and still looks plausible — which is why
/// [`temporality`](Self::temporality) is read off the wire rather than assumed
/// (ADR-0007).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtelMetric {
    /// Which counter.
    pub name: MetricName,
    /// The datapoint value.
    pub value: f64,
    /// Read from the wire, never assumed:
    /// `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE` can flip it.
    pub temporality: Temporality,
    /// Remaining attributes, PII already stripped.
    pub attributes: serde_json::Map<String, serde_json::Value>,
}

/// The eight counters, by wire name minus the `claude_code.` prefix.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MetricName {
    /// `claude_code.session.count`.
    SessionCount,
    /// `claude_code.lines_of_code.count`.
    LinesOfCode,
    /// `claude_code.pull_request.count`.
    PullRequestCount,
    /// `claude_code.commit.count`.
    CommitCount,
    /// `claude_code.cost.usage`. Its `model` attribute carries the `[1m]`
    /// context-window suffix that the event side does not.
    CostUsage,
    /// `claude_code.token.usage`.
    TokenUsage,
    /// `claude_code.code_edit_tool.decision`.
    CodeEditToolDecision,
    /// `claude_code.active_time.total`. A **counter** despite the `.total` name.
    ActiveTime,
    /// Anything else.
    Other(String),
}

/// OTLP aggregation temporality, as read off the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Temporality {
    /// The increment since the last export. Claude Code's default; **accumulate**.
    Delta,
    /// A running total. **Assign**.
    Cumulative,
    /// Neither was declared. Treat as [`Temporality::Delta`] and raise
    /// [`ControlEvent::SchemaDrift`].
    Unspecified,
}

/// Every `claude_code.*` **log event** name in the reference, minus the prefix
/// (`docs/verified/otel-schema.md` §5.1).
///
/// The dispatch table for `LogRecord` bodies: a `body_name` that is not in here
/// is [`OtelEvent::Unknown`] and a [`ControlEvent::SchemaDrift`]. Sorted, so a
/// binary search is available and a duplicate is a compile-time-visible mistake.
/// The eight metric names (see [`MetricName`]) and the five span names (see
/// [`OTEL_SPAN_NAMES`]) are deliberately **not** in this list — they arrive on
/// different OTLP services.
pub const OTEL_EVENT_NAMES: &[&str] = &[
    "api_error",
    "api_refusal",
    "api_request",
    "api_request_body",
    "api_response_body",
    "api_retries_exhausted",
    "assistant_response",
    "at_mention",
    "auth",
    "compaction",
    "feedback_survey",
    "hook_execution_complete",
    "hook_execution_start",
    "hook_plugin_metrics",
    "hook_registered",
    "internal_error",
    "mcp_server_connection",
    "permission_mode_changed",
    "plugin_installed",
    "plugin_loaded",
    "retention_sweep",
    "skill_activated",
    "subagent_completed",
    "tool_decision",
    "tool_result",
    "user_prompt",
];

/// Every `claude_code.*` **span** name, full and unabbreviated
/// (`docs/verified/otel-schema.md` §6).
///
/// Spans arrive on `TraceService`, which is why they are listed separately from
/// [`OTEL_EVENT_NAMES`]. `claude_code.hook` is present for completeness and must
/// never be enabled: capturing it requires *detailed* beta tracing, which
/// redirects logs and traces to a separate endpoint and would hijack Polis's own
/// export destination (ADR-0006).
pub const OTEL_SPAN_NAMES: &[&str] = &[
    "claude_code.hook",
    "claude_code.interaction",
    "claude_code.llm_request",
    "claude_code.tool",
    "claude_code.tool.blocked_on_user",
    "claude_code.tool.execution",
];

/// Attribute names Polis refuses by name at the parser boundary.
///
/// These carry entire conversation histories. They only appear under
/// `OTEL_LOG_RAW_API_BODIES`, which Polis never sets — but a user may, and Polis
/// must not persist them regardless (ADR-0005).
pub const REFUSED_EVENT_NAMES: &[&str] = &["api_request_body", "api_response_body"];

/// Attributes dropped at ingest because they are personally identifying.
///
/// `user.email` rides on **every event and every metric datapoint** by default
/// and no environment variable suppresses it. Polis needs a stable agent
/// identity, not an identity document.
pub const PII_ATTRIBUTES: &[&str] = &[
    "user.email",
    "user.account_uuid",
    "user.account_id",
    "user.groups",
];

// ---------------------------------------------------------------------------
// Channel B — Hooks
// ---------------------------------------------------------------------------

/// One hook datagram, decoded (PRD §4.2).
///
/// The variant set is [`EventKind`], which enumerates exactly the 19 events
/// `polis install-hooks` registers — `WorktreeCreate` deliberately excluded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEvent {
    /// Authoritative kind, taken from `hook_event_name` in the payload. The wire
    /// tag is only a routing hint.
    pub kind: EventKind,
    /// Set when the sender cut the payload at 60 KiB. Treat as
    /// notification-only and backfill from the transcript.
    pub truncated: bool,
    /// The parsed payload.
    pub payload: HookPayload,
}

/// The fields every hook payload shares (`docs/verified/hooks-schema.md` §2).
///
/// Everything is `Option` and the untyped remainder is preserved, because the
/// payload shape differs per event and moves between Claude Code releases.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookPayload {
    /// Current session. **Stays the parent session's id inside a subagent.**
    #[serde(default)]
    pub session_id: Option<SessionId>,
    /// The Channel A ↔ Channel B join key. Requires v2.1.196+ and is absent
    /// until the first user input, so `SessionStart` never has one.
    #[serde(default)]
    pub prompt_id: Option<PromptId>,
    /// Path to the conversation JSONL. Written asynchronously and **may lag the
    /// in-memory conversation**.
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// Working directory at invocation. Follows Claude into a worktree and after
    /// `cd`, which is what makes it usable for PRD §7.6 root discovery.
    #[serde(default)]
    pub cwd: Option<String>,
    /// `default` / `plan` / `acceptEdits` / `auto` / `dontAsk` /
    /// `bypassPermissions`. The UI's **Manual** mode arrives as `default`.
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// The event's own name — authoritative over the wire tag.
    #[serde(default)]
    pub hook_event_name: Option<String>,
    /// Present **only** inside a subagent. Presence is the worker discriminator.
    #[serde(default)]
    pub agent_id: Option<WorkerId>,
    /// Agent type. Present for `--agent` sessions too, so it is not a
    /// discriminator.
    #[serde(default)]
    pub agent_type: Option<AgentType>,
    /// The tool, on `PreToolUse` / `PostToolUseFailure` / `PermissionRequest`.
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Raw tool input. `file_path` lives in here for the mutating tools; this is
    /// the **only authoritative pre-execution path source** Polis has, because
    /// `tool_decision` on the OTel channel carries no path (PRD §11.3, ADR-0004).
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    /// Subagent transcript path on `SubagentStop` — authoritative, and it
    /// arrives with `agent_id` already attached, so the tailer discovers
    /// subagent files from here rather than by globbing.
    #[serde(default)]
    pub agent_transcript_path: Option<String>,
    /// Everything else, kept verbatim (PRD §4.4's catch-all rule).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl HookPayload {
    /// Parses a hook datagram body.
    ///
    /// The payload is opaque bytes to `polis-hook`; this is where it first gets
    /// parsed, on the daemon side, exactly as PRD §4.2 requires.
    pub fn parse(body: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(body)
    }

    /// True when the hook fired inside a subagent.
    pub fn is_worker(&self) -> bool {
        self.agent_id.is_some()
    }

    /// The authoritative event kind, from `hook_event_name`.
    pub fn kind(&self) -> EventKind {
        self.hook_event_name
            .as_deref()
            .map_or(EventKind::Unknown, EventKind::from_name)
    }
}

// ---------------------------------------------------------------------------
// Channel C — filesystem
// ---------------------------------------------------------------------------

/// A mutation observed on disk (PRD §4.3).
///
/// > The filesystem does not know which agent wrote.
///
/// So these never drive an alert on their own. Attribution comes from
/// correlating against the tool-call stream, anchored on the hook or OTel clock;
/// where it must be certain, it comes from a `PreToolUse` claim. The
/// `FileChanged` hook is **not** an alternative: it watches a literal filename
/// list and carries no `tool_name` or `tool_use_id`, so it is exactly as
/// attribution-blind as this channel while additionally costing a process spawn
/// (ADR-0003).
///
/// `#[non_exhaustive]`: `notify` 9 is already in release-candidate and its event
/// vocabulary is the thing most likely to grow (permission changes, hard-link
/// events). Reducing a new one into this enum must not break `polis-world`
/// (ADR-0048).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FsEvent {
    /// A file appeared.
    Created {
        /// Where.
        path: (WorktreeId, LogicalPath),
    },
    /// A file's contents changed.
    Modified {
        /// Where.
        path: (WorktreeId, LogicalPath),
    },
    /// A file disappeared. Leaves a vacant lot that goes to seed (PRD §7.5).
    Removed {
        /// Where.
        path: (WorktreeId, LogicalPath),
    },
    /// A file moved. Both ends are reported so the building can be relocated
    /// rather than demolished and rebuilt.
    Renamed {
        /// Old location.
        from: (WorktreeId, LogicalPath),
        /// New location.
        to: (WorktreeId, LogicalPath),
    },
    /// The watcher lost events and the tree must be re-scanned. `notify` reports
    /// this when the OS queue overflows; ignoring it silently desynchronises the
    /// city from disk.
    RescanRequired,
}

// ---------------------------------------------------------------------------
// Channel D — JSONL transcripts
// ---------------------------------------------------------------------------

/// One transcript record (PRD §4.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEvent {
    /// Which record type.
    pub kind: TranscriptRecordKind,
    /// The file it came from.
    pub source: TranscriptSource,
    /// Byte offset within that file. **This, not the timestamp, is the record's
    /// order**: 20% of transcript files contain a backwards timestamp step and
    /// one observed jump was 60 seconds.
    pub byte_offset: u64,
    /// The record itself, parsed only as far as PRD §4.4's
    /// `{ known fields } + Value` rule requires.
    pub record: serde_json::Value,
}

/// Which transcript file a record came from.
///
/// A session is a **forest**, not a tree: the main transcript plus one
/// independently-rooted tree per subagent, with no `parentUuid` edge crossing
/// between files. Cross-file parenthood is carried by `agent-<id>.meta.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TranscriptSource {
    /// `<munged-cwd>/<session-id>.jsonl`.
    Main,
    /// `<session-id>/subagents/[workflows/wf_<runId>/]agent-<agentId>.jsonl`.
    ///
    /// Variable depth: `[]` for a directly spawned agent, `["workflows",
    /// "wf_<runId>"]` for a workflow-spawned one. Glob for `agent-*.jsonl`;
    /// never assume a fixed depth.
    Subagent {
        /// The agent whose transcript this is.
        agent: WorkerId,
        /// The `<runId>` from a `wf_<runId>` path segment, when the agent was
        /// spawned by a `Workflow` call. `None` for a directly spawned agent.
        ///
        /// **Load-bearing, not decoration.** 503 of 643 subagent transcripts are
        /// workflow ones, and they carry neither `toolUseId` nor
        /// `parentAgentId`: this run id matched against the parent `Workflow`
        /// call's `toolUseResult.runId` is their *only* parent link (ADR-0013,
        /// 40/40 on disk). Dropping it orphans four out of five subagents.
        workflow_run: Option<String>,
    },
    /// `<session-id>/subagents/workflows/wf_<runId>/journal.jsonl` — the
    /// cheapest liveness signal available for a workflow fleet.
    ///
    /// A `started` record with no matching `result` is a subagent still running,
    /// or crashed.
    WorkflowJournal {
        /// The `<runId>` from the enclosing `wf_<runId>` directory. Without it
        /// two concurrent workflow runs in one session are indistinguishable.
        run: String,
    },
}

/// The 19 record types observed across 866 files and ~153 613 records
/// (`docs/verified/jsonl-schema.md` §2).
///
/// Only the first four are **threaded** (`uuid` + `parentUuid`). The other 15
/// are flat session sidecars with no envelope and no position in any tree; about
/// 25% of all lines have no `uuid` at all, so a parser that assumes one breaks
/// on a quarter of the corpus.
///
/// `#[non_exhaustive]`: the format is undocumented internals and the count went
/// from "a handful" to 19 inside the observed release window. A twentieth must
/// arrive as [`TranscriptRecordKind::Unknown`] plus a drift signal, not as a
/// compile error in four crates (ADR-0048).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum TranscriptRecordKind {
    /// `user` — threaded. Carries `tool_result` blocks and `toolUseResult`.
    User,
    /// `assistant` — threaded. Carries `tool_use`, `text` and `thinking` blocks.
    Assistant,
    /// `attachment` — threaded. The only record of a file the operator
    /// referenced with `@`, which produces no `Read` tool call.
    Attachment,
    /// `system` — threaded. `compact_boundary` records live here; after one, the
    /// pre-compaction thread is reachable only via `logicalParentUuid`.
    System,
    /// `mode`.
    Mode,
    /// `last-prompt`. `leafUuid` names the live leaf of the branching tree.
    LastPrompt,
    /// `bridge-session`.
    BridgeSession,
    /// `permission-mode`.
    PermissionMode,
    /// `ai-title`.
    AiTitle,
    /// `custom-title`.
    CustomTitle,
    /// `agent-name`.
    AgentName,
    /// `queue-operation`.
    QueueOperation,
    /// `file-history-delta`.
    FileHistoryDelta,
    /// `file-history-snapshot`.
    FileHistorySnapshot,
    /// `started` — `journal.jsonl` only. A `started` with no matching `result`
    /// is a subagent still running or crashed.
    Started,
    /// `result` — `journal.jsonl` only.
    Result,
    /// `atis-latch`.
    AtisLatch,
    /// `frame-link`.
    FrameLink,
    /// `cost-state`. Carries `totalLinesAdded` / `totalLinesRemoved` per session,
    /// one of the three free sources for PRD §7.3's building height.
    CostState,
    /// A record type this Claude Code release added. Counted as drift, never
    /// fatal.
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Polis's own health
// ---------------------------------------------------------------------------

/// Polis's own health signals, surfaced in the status bar (PRD §4.5, §17).
///
/// > a schema-drift warning in the status bar rather than a crash.
///
/// `#[non_exhaustive]`: this is the list of things that can go wrong, and it
/// grows every time a new failure mode is found in the field. The status bar
/// having no rendering for a new one is a cosmetic gap; a compile break across
/// the workspace is not (ADR-0048).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ControlEvent {
    /// A channel is unavailable and Polis is running without it.
    ///
    /// The canonical cases: port 4317 already in use by a stale collector or a
    /// second Polis (log a warning, continue without Channel A, **never** pick a
    /// different port — agents are configured to talk to 4317); and the beta
    /// traces channel producing no spans, in which case every tool call is
    /// attributed to the main agent and the session is marked degraded rather
    /// than guessed at.
    ChannelDegraded {
        /// Which channel.
        channel: Channel,
        /// Operator-readable reason.
        reason: String,
    },
    /// The bus dropped events (PRD §4.5). Backpressure must never propagate to
    /// an agent, so dropping is correct — but it must be visible.
    EventsDropped {
        /// How many since the last report.
        count: u64,
        /// Where the drop happened.
        channel: Channel,
    },
    /// A hole in `event.sequence`: telemetry was lost between Claude Code and
    /// Polis. Free loss detection, since the counter is monotonic per session.
    SequenceGap {
        /// The session with the hole.
        session: SessionId,
        /// The sequence number expected next.
        expected: u64,
        /// The one that actually arrived.
        got: u64,
    },
    /// A field, event or record type Polis does not model appeared.
    SchemaDrift {
        /// Which channel drifted.
        channel: Channel,
        /// The Claude Code version that produced it, when known — every
        /// observation in the recon documents is pinned to a release.
        producer_version: Option<String>,
        /// What was unrecognised.
        detail: String,
    },
    /// Ingest is shutting down; the world thread should flush and stop.
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hook_payload_parses_with_only_the_fields_that_arrived() {
        // Shape taken from docs/verified/hooks-schema.md §3.3, main-agent case.
        let body = br#"{
            "session_id": "68160373-1111-2222-3333-444455556666",
            "transcript_path": "C:\\Users\\op\\.claude\\projects\\x\\y.jsonl",
            "cwd": "C:\\coding\\agentolis",
            "hook_event_name": "PreToolUse",
            "permission_mode": "default",
            "tool_name": "Edit",
            "tool_input": {"file_path": "C:\\coding\\agentolis\\src\\a.rs"},
            "something_new_in_2_1_260": 42
        }"#;
        let p = HookPayload::parse(body).expect("must parse");
        assert_eq!(p.kind(), EventKind::PreToolUse);
        assert!(
            !p.is_worker(),
            "no agent_id means main agent, not a parse failure"
        );
        assert_eq!(p.tool_name.as_deref(), Some("Edit"));
        assert_eq!(
            p.session_id.as_ref().unwrap().as_str(),
            "68160373-1111-2222-3333-444455556666"
        );
        // The unknown field is preserved rather than rejected (PRD §4.4).
        assert_eq!(
            p.extra
                .get("something_new_in_2_1_260")
                .and_then(serde_json::Value::as_u64),
            Some(42)
        );
        // Absent fields are absent, not defaults that look real.
        assert!(
            p.prompt_id.is_none(),
            "SessionStart-era payloads have no prompt_id"
        );
    }

    #[test]
    fn agent_id_presence_is_the_worker_discriminator() {
        // A main agent launched with `claude --agent foo` carries agent_type and
        // no agent_id. Keying on agent_type would call it a worker.
        let main_with_agent_flag =
            br#"{"hook_event_name":"Stop","agent_type":"Explore","session_id":"s"}"#;
        let p = HookPayload::parse(main_with_agent_flag).unwrap();
        assert!(p.agent_type.is_some());
        assert!(!p.is_worker());

        let subagent = br#"{"hook_event_name":"Stop","agent_type":"Explore",
                           "agent_id":"a106e5fe86fce476a","session_id":"s"}"#;
        let p = HookPayload::parse(subagent).unwrap();
        assert!(p.is_worker());
        assert_eq!(p.agent_id.unwrap().as_str(), "a106e5fe86fce476a");
    }

    #[test]
    fn an_unmodelled_hook_name_degrades_to_unknown() {
        let p = HookPayload::parse(br#"{"hook_event_name":"WorktreeCreate"}"#).unwrap();
        assert_eq!(p.kind(), EventKind::Unknown);
        let p = HookPayload::parse(b"{}").unwrap();
        assert_eq!(p.kind(), EventKind::Unknown);
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        for body in [
            b"".as_slice(),
            b"{",
            b"null",
            b"[]",
            b"\xff\xfe",
            b"{\"a\":",
        ] {
            let _ = HookPayload::parse(body);
        }
        assert!(HookPayload::parse(b"{").is_err());
        // A bare JSON value that is not an object is also an error, not a
        // half-populated struct.
        assert!(HookPayload::parse(b"[]").is_err());
    }

    #[test]
    fn transcript_record_kinds_use_the_wire_spellings() {
        let cases = [
            ("user", TranscriptRecordKind::User),
            ("last-prompt", TranscriptRecordKind::LastPrompt),
            (
                "file-history-snapshot",
                TranscriptRecordKind::FileHistorySnapshot,
            ),
            ("cost-state", TranscriptRecordKind::CostState),
            ("bridge-session", TranscriptRecordKind::BridgeSession),
        ];
        for (wire, kind) in cases {
            let parsed: TranscriptRecordKind =
                serde_json::from_str(&format!("\"{wire}\"")).unwrap();
            assert_eq!(parsed, kind, "{wire}");
        }
        // A future record type is drift, not a parse failure.
        let parsed: TranscriptRecordKind = serde_json::from_str("\"invented-in-2-2-0\"").unwrap();
        assert_eq!(parsed, TranscriptRecordKind::Unknown);
    }

    /// ADR-0013: a workflow subagent's only parent link is its `wf_<runId>`,
    /// so two runs in one session must not collapse onto one source.
    #[test]
    fn a_workflow_run_id_distinguishes_two_concurrent_fleets() {
        let a = TranscriptSource::Subagent {
            agent: WorkerId::new("a96cf8a57447af436"),
            workflow_run: Some("01JQ".into()),
        };
        let b = TranscriptSource::Subagent {
            agent: WorkerId::new("a96cf8a57447af436"),
            workflow_run: Some("01JR".into()),
        };
        let direct = TranscriptSource::Subagent {
            agent: WorkerId::new("a96cf8a57447af436"),
            workflow_run: None,
        };
        assert_ne!(a, b, "two runs of one agent type are not one source");
        assert_ne!(a, direct, "a directly spawned agent is not a workflow one");
        assert_ne!(
            TranscriptSource::WorkflowJournal { run: "01JQ".into() },
            TranscriptSource::WorkflowJournal { run: "01JR".into() }
        );
        assert_ne!(TranscriptSource::Main, direct);
    }

    #[test]
    fn meta_derives_the_thread_from_the_session() {
        let m = EventMeta::now(Channel::Hook).with_session(SessionId::new("s1"));
        assert_eq!(m.thread.as_ref().unwrap().as_str(), "s1");
        assert!(!m.is_worker());
    }

    #[test]
    fn the_refusal_and_pii_lists_are_not_empty_by_accident() {
        assert!(REFUSED_EVENT_NAMES.contains(&"api_request_body"));
        assert!(REFUSED_EVENT_NAMES.contains(&"api_response_body"));
        assert!(PII_ATTRIBUTES.contains(&"user.email"));
        // The refused names must be inside the dispatch table, or the refusal
        // never runs — the table is what a receiver matches on first.
        for name in REFUSED_EVENT_NAMES {
            assert!(
                OTEL_EVENT_NAMES.contains(name),
                "{name} is not dispatchable"
            );
        }
    }

    /// `docs/verified/otel-schema.md` §5.1 counts 39 `claude_code.*` names: 8
    /// metrics, 5 spans, 26 events. If any of those three counts moves, a
    /// Claude Code release changed the schema and the dispatch tables are stale.
    #[test]
    fn the_otel_dispatch_tables_are_complete_and_sorted() {
        assert_eq!(OTEL_EVENT_NAMES.len(), 26, "§5.1 counts 26 event names");
        let mut sorted = OTEL_EVENT_NAMES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, OTEL_EVENT_NAMES, "duplicate or unsorted event name");

        // 5 span names plus `claude_code.hook`, which is listed but never enabled.
        assert_eq!(OTEL_SPAN_NAMES.len(), 6);
        assert!(OTEL_SPAN_NAMES
            .iter()
            .all(|n| n.starts_with("claude_code.")));
        let mut sorted = OTEL_SPAN_NAMES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, OTEL_SPAN_NAMES, "duplicate or unsorted span name");

        // The two tables must not overlap: they arrive on different services and
        // a name in both would dispatch twice.
        for e in OTEL_EVENT_NAMES {
            assert!(!OTEL_SPAN_NAMES.contains(e));
        }
    }

    /// Every span in §6.1's hierarchy has a home. Before this, only
    /// `claude_code.tool` did, so a subagent's `llm_request` — one of exactly
    /// two places `agent_id` appears — degraded to `Unknown`.
    #[test]
    fn every_modelled_span_has_a_variant() {
        let spans = [
            OtelEvent::InteractionSpan {
                span_id: Some("1b69803d0429f2e8".into()),
                sequence: Some(0),
                duration_ms: Some(1.0),
                prompt_length: Some(42),
            },
            OtelEvent::LlmRequestSpan {
                span_id: None,
                parent_span_id: None,
                model: None,
                context: Some("standalone".into()),
                duration_ms: None,
                outcome: Outcome::Pending,
            },
            OtelEvent::ToolSpan {
                call: Box::new(ToolCall {
                    tool: ToolKind::Edit,
                    tool_use_id: None,
                    paths: Vec::new(),
                    outcome: Outcome::Done,
                    duration_ms: None,
                }),
                span_id: None,
                parent_span_id: None,
            },
            OtelEvent::ToolExecutionSpan {
                tool_use_id: None,
                parent_span_id: None,
                duration_ms: None,
                outcome: Outcome::Failed,
            },
            OtelEvent::ToolBlockedOnUserSpan {
                tool_use_id: None,
                parent_span_id: None,
                duration_ms: Some(9_000.0),
            },
        ];
        // One variant per span name except `claude_code.hook`, which Polis
        // must never enable.
        assert_eq!(spans.len(), OTEL_SPAN_NAMES.len() - 1);
        for s in &spans {
            assert!(!matches!(s, OtelEvent::Unknown { .. }));
        }
    }

    #[test]
    fn payload_stays_small_enough_for_a_65k_deep_queue() {
        // 65 536 entries at, say, 700 bytes would be 45 MB of resident memory
        // for the queue alone. The boxes are what keep this honest.
        assert!(
            std::mem::size_of::<Payload>() <= 64,
            "Payload grew to {} bytes; box the new variant",
            std::mem::size_of::<Payload>()
        );
    }
}
