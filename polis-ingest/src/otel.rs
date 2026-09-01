//! Channel A — the embedded OTLP/gRPC receiver (PRD §4.1).
//!
//! > Polis embeds an OTLP/gRPC receiver on 4317 (`tonic` + `opentelemetry-proto`).
//! > No external collector.
//!
//! # Register three services, not two
//!
//! `LogsServiceServer`, `MetricsServiceServer` **and** `TraceServiceServer`.
//! Omitting the third makes Claude Code's trace exports fail `UNIMPLEMENTED` and
//! costs Polis its thread model, because the logs channel carries no subagent
//! discriminator at all (ADR-0006). If spans stop arriving, attribute every tool
//! call to the main agent and mark the session degraded — never guess.
//!
//! # Bind synchronously, then hand the listener to tokio
//!
//! `std::net::TcpListener::bind` on the calling thread, then `from_std` inside
//! the runtime. This is the whole trick: `AddrInUse` surfaces as a matchable
//! `io::Error` instead of a panic buried in a background task nobody joins.
//! Never retry-loop, and never pick a different port — agents are configured to
//! talk to 4317, so a different port yields an empty city that looks healthy.
//! The Windows error *string* is localised, so match on
//! [`std::io::ErrorKind::AddrInUse`], never on the message.
//!
//! # Three things that are easy to get quietly wrong
//!
//! * **`session.id` is not a resource attribute.** The resource block carries
//!   exactly five keys — `service.name`, `service.version`, `os.type`,
//!   `os.version`, `host.arch`. Grouping by resource, the normal OTLP idiom,
//!   cannot separate agents.
//! * **Counters are DELTA.** Every export is the increment since the last.
//!   Reading them as cumulative is a *silent* failure: totals collapse to "the
//!   last export interval" and still look plausible (ADR-0007).
//! * **`tool_input` is a JSON string, not a kvlist.** A receiver that only
//!   flattens OTLP attributes recovers **zero** file paths, and on Windows the
//!   embedded separators are JSON-escaped and must be decoded, not sliced.

use std::net::SocketAddr;

use polis_events::{Event, PathMapper};

use crate::bus::EventSink;

/// The OTLP receiver and its private tokio runtime.
#[derive(Debug)]
pub struct OtlpReceiver {
    _private: (),
}

impl OtlpReceiver {
    /// Binds `addr` synchronously, then serves on a private two-worker runtime
    /// running on a named background thread.
    ///
    /// Two workers is ample: the handler is ~50 µs of pure decode and traffic is
    /// roughly one batch per second per agent.
    pub fn start(
        addr: SocketAddr,
        sink: EventSink,
        mapper: PathMapper,
    ) -> Result<Self, std::io::Error> {
        let _ = (addr, sink, mapper);
        todo!("PRD §4.1 — bind eagerly, then serve Logs + Metrics + Trace")
    }

    /// Stops the runtime and joins its thread.
    pub fn shutdown(self) {
        todo!("PRD §4.1 — signal the tonic server, then join the runtime thread")
    }
}

/// Renders any OTLP `AnyValue` variant to a string.
///
/// **Coerce by key, accepting any scalar variant** — do not match one variant
/// per key. Wire types vary per `(event, key)` pair: `duration_ms` is a string on
/// `mcp_server_connection` and an int on `tool_result`; `safe_mode` is the string
/// `"false"` while `has_hooks` is a real bool in the same event. A decoder that
/// matches one variant per key loses fields with no error and no log line
/// (ADR-0015).
///
/// Must handle every arm including the profiling-only `string_value_strindex`
/// and the absent case.
pub fn any_value_to_string(value: &opentelemetry_proto::tonic::common::v1::AnyValue) -> String {
    let _ = value;
    todo!("docs/verified/otlp-receiver.md — total over every AnyValue arm")
}

/// Recovers file paths from a `tool_input` / `tool_parameters` JSON string.
///
/// Searches `file_path`, `notebook_path`, `path` and `target_file`. Combined with
/// the finding that the `FileChanged` hook cannot attribute, this is the only
/// reliable path-to-agent link on the OTel channel — load-bearing for territory
/// inference, not a detail.
///
/// Values inside `tool_input` are truncated per-value at 128 characters with a
/// `…[N chars]` marker (the docs say 512; observed is 128). `file_path` is
/// unaffected because it is the first and shortest key, but nothing else here may
/// be assumed complete.
pub fn tool_input_paths(tool_input_json: &str) -> Vec<String> {
    let _ = tool_input_json;
    todo!("PRD §4.3 — JSON-decode, never string-slice; Windows separators are escaped")
}

/// Converts one decoded OTLP record into a bus event, dropping PII on the way.
///
/// Returns `None` for records Polis does not model and for anything whose
/// `service.name` is not `claude-code` — a cheap filter against foreign OTLP
/// traffic on a loopback port that anyone can post to.
pub fn normalize_record(record: &OtlpRecord<'_>, mapper: &PathMapper) -> Option<Event> {
    let _ = (record, mapper);
    todo!("PRD §4.1 — drop user.email, refuse api_request_body/api_response_body by name")
}

/// A flattened OTLP record, before it becomes an [`Event`].
///
/// Borrowed rather than owned so the decode path allocates once per batch
/// instead of once per attribute.
#[derive(Debug)]
pub struct OtlpRecord<'a> {
    /// The event name from the `LogRecord` **body** (`claude_code.tool_result`).
    /// The OTLP 1.7 `event_name` field is empty on every record, and
    /// `severity_number` is `UNSPECIFIED` on every record — neither can dispatch.
    pub body_name: &'a str,
    /// Flattened attributes, nested kvlists joined with dots.
    pub attributes: &'a [(String, String)],
    /// `trace_id`, present once the beta traces channel is enabled. Enabling
    /// traces retro-fits these onto log records, so a subagent's `tool_result`
    /// carries a different span from the main agent's.
    pub trace_id: Option<&'a str>,
    /// `span_id`, same story.
    pub span_id: Option<&'a str>,
}
