//! `polis-events` — the shared contract crate (PRD §14).
//!
//! Everything that crosses a crate boundary in Polis is defined here: the
//! [`Event`] enum the four ingest channels normalise into (PRD §4.5), the hook
//! wire format `polis-hook` writes and `polis-ingest` reads (PRD §4.2), and
//! [`LogicalPath`] — the layout key every other crate maps files onto (PRD §7.6).
//!
//! # Why this crate is deliberately dull
//!
//! Eight crates depend on these types. A mistake here is expensive in a way a
//! mistake in, say, the road-growth algorithm is not: it propagates into every
//! `HashMap` key, every golden file, and every wire frame. So this crate holds
//! **no policy and no algorithms** — only shapes, the wire codec, and path
//! normalisation, all of which are implemented and tested rather than stubbed.
//!
//! # Defensive parsing is a hard requirement
//!
//! PRD §4.1 and §4.4 both mandate it, and the recon work found that the real
//! failure mode is subtler than "unknown event type":
//!
//! * **Absence is often meaningful, not an error.** No `agent_id` on a hook
//!   payload means *main agent*, not *parse failure*
//!   (`docs/verified/hooks-schema.md` §2.1).
//! * **Wire types vary per (event, key) pair.** `duration_ms` arrives as an OTLP
//!   string on `mcp_server_connection` and as an int on `tool_result`;
//!   `safe_mode` is the string `"false"` while `has_hooks` is a real bool in the
//!   same event (`docs/verified/otlp-receiver.md`).
//! * **Every field is `Option`.** Nothing on any of the four channels is a
//!   contract; all of it is beta or undocumented internals.
//!
//! Accordingly, no decoder in this crate may panic on any input. The wire
//! decoder in [`wire`] is fuzz-shaped by construction: it validates lengths
//! before slicing and returns [`wire::WireError`] rather than unwinding.

//! # Two rules that bind every downstream crate
//!
//! 1. **The drift-carrying enums are `#[non_exhaustive]`** — [`EventKind`],
//!    [`ToolKind`], [`OtelEvent`], [`TranscriptRecordKind`], [`FsEvent`],
//!    [`Payload`] and [`ControlEvent`]. Every one of them models a beta or
//!    undocumented Claude Code schema that is expected to gain members, and with
//!    eight crates matching on them, an addition would otherwise be a
//!    workspace-wide compile break rather than the drift signal it is. A `match`
//!    on any of them outside this crate needs a wildcard arm; that arm is where a
//!    [`ControlEvent::SchemaDrift`] belongs (ADR-0048).
//! 2. **Serialization goes through [`record::RecordedEvent`]**, never through a
//!    bare [`Event`]. [`EventMeta::observed`] is an `Instant` — right for live
//!    ordering, meaningless on disk — so it is skipped by serde and the recorded
//!    form carries a wall clock plus a monotonic offset instead (ADR-0049).

pub mod event;
pub mod ids;
pub mod kind;
pub mod path;
pub mod record;
pub mod tool;
pub mod wire;

pub use event::{
    Channel, ControlEvent, Event, EventMeta, FsEvent, HookEvent, HookPayload, MetricName,
    OtelEvent, OtelMetric, Payload, Temporality, ToolCall, TranscriptEvent, TranscriptRecordKind,
    TranscriptSource, OTEL_EVENT_NAMES, OTEL_SPAN_NAMES, PII_ATTRIBUTES, REFUSED_EVENT_NAMES,
};
pub use ids::{AgentType, PromptId, SessionId, ThreadId, ToolUseId, WorkerId, WorktreeId};
pub use kind::EventKind;
pub use path::{LogicalPath, PathMapper, PathParseError};
pub use record::{
    RecordedEvent, RecordingClock, RecordingHeader, ReplayClock, WallTime, RECORDING_FORMAT,
};
pub use tool::{Glyph, Outcome, ToolKind};
pub use wire::{
    encode_frame, WireError, WireHeader, DEFAULT_HOOK_PORT, HEADER_LEN, HOOK_ENDPOINT_ENV,
    MAX_DATAGRAM, MAX_PAYLOAD, RECV_BUFFER_BYTES, TAG_KIND_MASK, TAG_TRUNCATED,
};

/// Capacity of the bounded event bus (PRD §4.5).
///
/// The bus drops the **oldest** entry when full, which `crossbeam` does not do
/// for you: `try_send` on a full bounded channel drops the *newest*. The sink
/// must hold a `Receiver` clone and `try_recv()` to evict before retrying
/// (`docs/verified/otlp-receiver.md`; ADR-0008). Backpressure must never reach
/// an agent.
pub const EVENT_BUS_CAPACITY: usize = 65_536;
