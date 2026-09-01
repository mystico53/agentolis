//! Normalisation — raw record to [`Event`], for all four channels (PRD §4.5).
//!
//! > All four channels normalize into one internal `Event` enum and push into a
//! > bounded `crossbeam` channel.
//!
//! Everything here is a **pure function of its input plus a [`PathMapper`]**.
//! That is deliberate and load-bearing:
//!
//! * PRD §16 fuzzes the transcript parser and drives the hook-safety matrix.
//!   Both need to call the decode path directly, with no socket, no runtime and
//!   no filesystem.
//! * The PII and refusal rules (ADR-0005) must be applied in exactly one place.
//!   `user.email` rides on **every** event and every metric datapoint by default
//!   and no environment variable suppresses it; `api_request_body` and
//!   `api_response_body` carry entire conversation histories. A second copy of
//!   the scrubbing logic is a second place for it to be forgotten.
//! * Path normalisation (PRD §7.6) is the layout key. Two channels computing it
//!   slightly differently produces two cities.
//!
//! # No function here may panic
//!
//! Every input is beta or undocumented, arrives over an unauthenticated loopback
//! port, or is read from a file another process is appending to. Return `None`
//! or a drift signal; never unwrap, never slice a `str` by an index derived from
//! another string (ADR-0046).
//!
//! # Defensive rules, per channel
//!
//! * **A.** Coerce attributes **by key, accepting any scalar variant**. Wire
//!   types vary per `(event, key)` pair: `duration_ms` is a string on
//!   `mcp_server_connection` and an int on `tool_result`; `safe_mode` is the
//!   string `"false"` while `has_hooks` is a real bool in the same event. A
//!   decoder that matches one variant per key loses fields with no error and no
//!   log line (ADR-0015).
//! * **B.** `hook_event_name` in the payload is authoritative; the wire tag is a
//!   routing hint. A set truncation bit means notification-only — backfill the
//!   detail from the transcript.
//! * **C.** The filesystem knows nothing about agents. Attribution happens in
//!   `polis-world`, anchored on the hook or OTel clock (ADR-0014), never here.
//! * **D.** Only 4 of the 19 record types are threaded and ~25% of lines have no
//!   `uuid`. Record order is **byte offset**, never the timestamp.
//!
//! # The shared helpers come first
//!
//! [`logical_path`], [`tool_kind`], [`outcome`], the `attr_*` coercions and the
//! timestamp conversions are the four channel modules' common vocabulary. They
//! are dependency-free by design: no `chrono`, no `regex`, no glob crate —
//! [`wall_from_rfc3339`] is forty lines of civil-calendar arithmetic rather than
//! a dependency, for the same reason `LogicalPath::layout_seed` writes out
//! FNV-1a instead of using `DefaultHasher` (ADR-0029). A timestamp is part of
//! the recorded format; a crate's behaviour is not something Polis controls.

use std::path::Path;

use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::metrics::v1::{metric, number_data_point, Metric};
use serde_json::Value;

use polis_events::{
    AgentType, Channel, ControlEvent, Event, EventMeta, FsEvent, HookEvent, LogicalPath,
    MetricName, OtelEvent, OtelMetric, Outcome, PathMapper, Payload, PromptId, SessionId,
    Temporality, ToolCall, ToolKind, ToolUseId, TranscriptSource, WallTime, WorkerId, WorktreeId,
    PII_ATTRIBUTES, REFUSED_EVENT_NAMES,
};

use crate::fswatch::is_watch_excluded;

/// The only `service.name` Polis accepts on the OTLP port.
///
/// Port 4317 on loopback is unauthenticated and anyone on the machine can post
/// to it; this is the cheap first filter against foreign traffic.
pub const CLAUDE_CODE_SERVICE: &str = "claude-code";

/// Where [`hook_event`] files the logical paths it recovered from `tool_input`.
///
/// ADR-0005 requires the raw `tool_input` be reduced to path keys before
/// anything stores it — a `Write`'s contains the whole file, an `Edit`'s the
/// diff. The recovered [`LogicalPath`]s have to live somewhere, and
/// `polis_events::HookPayload` is frozen, so they go into its `extra` map under
/// this key as `[{"worktree": u32, "path": "src/a.rs"}, …]`.
pub const NORMALISED_PATHS_KEY: &str = "polis.paths";

/// Keys inside a `tool_input` / `tool_parameters` object that name a file.
const PATH_KEYS: &[&str] = &["file_path", "notebook_path", "path", "target_file"];

/// How deep [`flatten_attributes`] and [`tool_input_paths`] will recurse.
///
/// Both walk attacker-shaped JSON off a loopback port. A cap is cheaper than a
/// stack probe and the real shapes are two levels deep at most.
const MAX_DEPTH: usize = 6;

// ---------------------------------------------------------------------------
// Shared helpers — the vocabulary all four channels normalise through
// ---------------------------------------------------------------------------

/// Maps an absolute path onto its worktree and logical path (PRD §7.6).
///
/// The worktree prefix is stripped here and nowhere else: `/repo-wt-3/src/a.ts`
/// and `/repo-wt-7/src/a.ts` are one logical file in two physical places, and
/// the whole point of the city is showing that as one building. `None` means
/// the path is outside every registered root — a `~/.claude` transcript, a temp
/// file — which is a normal condition, not an error.
pub fn logical_path(mapper: &PathMapper, path: &str) -> Option<(WorktreeId, LogicalPath)> {
    mapper.to_logical_str(path)
}

/// [`logical_path`] for a value that may be relative, resolved against `cwd`.
///
/// Tool inputs carry both forms: `Read`'s `file_path` is usually absolute while
/// `Grep`'s `path` is frequently relative to the hook payload's `cwd`.
pub fn logical_path_in(
    mapper: &PathMapper,
    cwd: Option<&str>,
    path: &str,
) -> Option<(WorktreeId, LogicalPath)> {
    mapper.resolve(cwd.map(Path::new), path)
}

/// Maps a wire tool name onto a [`ToolKind`].
///
/// Total, and the single place the mapping happens. **Both shells are shells**:
/// on a Windows box without Git Bash, Claude Code does not register the `Bash`
/// tool at all and every shell command arrives as `PowerShell`
/// (`docs/verified/hooks-schema.md` §9.7, ADR-0032). A matcher written against
/// `Bash` alone silently matches nothing on such a machine, which is why no
/// caller should compare tool-name strings itself — use this, then
/// [`ToolKind::is_shell`].
pub fn tool_kind(name: &str) -> ToolKind {
    ToolKind::parse(name)
}

/// True when a wire tool name is one of the two shells (ADR-0032).
///
/// A convenience over `tool_kind(name).is_shell()` so a caller holding only a
/// string never has to spell out `name == "Bash" || name == "PowerShell"` and
/// get half of it right.
pub fn is_shell_tool(name: &str) -> bool {
    tool_kind(name).is_shell()
}

/// Maps a channel's spelling of success onto an [`Outcome`] (PRD §10.2).
///
/// `success` arrives as the **strings** `"true"` / `"false"` on OTLP, so this
/// takes the raw attribute rather than a `bool`. Absent means
/// [`Outcome::Pending`] — the call is in flight or its result was not seen —
/// never [`Outcome::Failed`].
pub fn outcome(success: Option<&str>) -> Outcome {
    Outcome::from_success(success.and_then(parse_bool))
}

/// Parses any scalar spelling of a boolean (ADR-0015).
pub fn parse_bool(raw: &str) -> Option<bool> {
    let raw = raw.trim();
    for t in ["true", "1", "yes", "on"] {
        if raw.eq_ignore_ascii_case(t) {
            return Some(true);
        }
    }
    for f in ["false", "0", "no", "off"] {
        if raw.eq_ignore_ascii_case(f) {
            return Some(false);
        }
    }
    None
}

/// Parses any scalar spelling of a float.
pub fn parse_f64(raw: &str) -> Option<f64> {
    let value: f64 = raw.trim().parse().ok()?;
    value.is_finite().then_some(value)
}

/// Parses any scalar spelling of an unsigned integer.
///
/// Accepts `"42"` and `"42.0"` — the same key arrives as an OTLP int on one
/// event and a double on another — but refuses `"42.5"`, which is not the
/// integer it claims to be.
pub fn parse_u64(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if let Ok(value) = raw.parse::<u64>() {
        return Some(value);
    }
    let (int, frac) = raw.split_once('.')?;
    if frac.bytes().all(|b| b == b'0') {
        int.parse().ok()
    } else {
        None
    }
}

/// Looks a key up in a flattened attribute list.
pub fn attr<'a>(attributes: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// [`attr`], owned.
pub fn attr_owned(attributes: &[(String, String)], key: &str) -> Option<String> {
    attr(attributes, key).map(str::to_owned)
}

/// [`attr`], coerced to a boolean by [`parse_bool`].
pub fn attr_bool(attributes: &[(String, String)], key: &str) -> Option<bool> {
    attr(attributes, key).and_then(parse_bool)
}

/// [`attr`], coerced to a float by [`parse_f64`].
pub fn attr_f64(attributes: &[(String, String)], key: &str) -> Option<f64> {
    attr(attributes, key).and_then(parse_f64)
}

/// [`attr`], coerced to an unsigned integer by [`parse_u64`].
pub fn attr_u64(attributes: &[(String, String)], key: &str) -> Option<u64> {
    attr(attributes, key).and_then(parse_u64)
}

/// True for an attribute Polis drops at ingest because it identifies a person
/// (ADR-0005).
pub fn is_pii_attribute(key: &str) -> bool {
    PII_ATTRIBUTES.contains(&key)
}

/// True for an event Polis refuses **by name**, before decoding it (ADR-0005).
pub fn is_refused_event(body_name: &str) -> bool {
    REFUSED_EVENT_NAMES.contains(&body_name)
}

/// A [`ControlEvent::SchemaDrift`] envelope, ready to push.
///
/// PRD §17 asks for *a schema-drift warning in the status bar rather than a
/// crash*, and every drift report should carry the Claude Code release that
/// produced it (ADR-0035) — which is why `producer_version` is a parameter and
/// not an afterthought.
pub fn drift(channel: Channel, producer_version: Option<&str>, detail: impl Into<String>) -> Event {
    Event::control(ControlEvent::SchemaDrift {
        channel,
        producer_version: producer_version.map(str::to_owned),
        detail: detail.into(),
    })
}

/// A wall-clock reading from OTLP's nanosecond epoch stamp.
pub fn wall_from_unix_nanos(nanos: u64) -> WallTime {
    let millis = i64::try_from(nanos / 1_000_000).unwrap_or(i64::MAX);
    WallTime::from_unix_millis(millis)
}

/// Parses an ISO-8601 / RFC-3339 timestamp — the spelling both the OTel
/// `event.timestamp` attribute and every transcript record use.
///
/// Accepts `2026-09-01T17:38:50.290Z`, a `+HH:MM` / `-HHMM` offset, a bare
/// naive timestamp (read as UTC), and any fractional precision. Returns `None`
/// rather than guessing on anything else.
///
/// **This is display-only on Channel D.** 20% of transcript files contain a
/// backwards timestamp step and one observed jump was 60 seconds backwards, so
/// nothing may be ordered or windowed on the result (ADR-0014).
pub fn wall_from_rfc3339(text: &str) -> Option<WallTime> {
    let text = text.trim();
    let (date, rest) = text.split_once(['T', 't', ' '])?;
    let (year, month, day) = split_date(date)?;
    let (time, offset_secs) = split_offset(rest)?;
    let (hours, minutes, seconds_text) = split_time(time)?;
    let (seconds, frac_ms) = split_fraction(seconds_text)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hours)
        || !(0..=59).contains(&minutes)
        // 60 is a leap second, which is a real value and not an error.
        || !(0..=60).contains(&seconds)
    {
        return None;
    }
    let secs = days_from_civil(year, month, day)
        .checked_mul(86_400)?
        .checked_add(hours * 3_600 + minutes * 60 + seconds)?
        .checked_sub(offset_secs)?;
    let millis = secs.checked_mul(1_000)?.checked_add(frac_ms)?;
    Some(WallTime::from_unix_millis(millis))
}

/// Splits `YYYY-MM-DD`.
fn split_date(date: &str) -> Option<(i64, i64, i64)> {
    let (year, rest) = date.split_once('-')?;
    let (month, day) = rest.split_once('-')?;
    Some((year.parse().ok()?, month.parse().ok()?, day.parse().ok()?))
}

/// Splits `HH:MM:SS[.fff]`, keeping the seconds as text so the fraction
/// survives.
fn split_time(time: &str) -> Option<(i64, i64, &str)> {
    let (hours, rest) = time.split_once(':')?;
    let (minutes, seconds) = rest.split_once(':')?;
    Some((hours.parse().ok()?, minutes.parse().ok()?, seconds))
}

/// Splits `SS.fff` into whole seconds and milliseconds, padding a short
/// fraction and truncating a long one.
fn split_fraction(text: &str) -> Option<(i64, i64)> {
    let Some((whole, frac)) = text.split_once('.') else {
        return Some((text.parse().ok()?, 0));
    };
    if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut millis = 0i64;
    for i in 0..3 {
        let digit = frac.as_bytes().get(i).map_or(0, |b| i64::from(b - b'0'));
        millis = millis * 10 + digit;
    }
    Some((whole.parse().ok()?, millis))
}

/// Splits the offset off the end of a time, returning its value in seconds.
///
/// A naive timestamp — no `Z`, no sign — is read as UTC, which is what every
/// producer Polis reads actually means.
fn split_offset(time: &str) -> Option<(&str, i64)> {
    if let Some(naive) = time.strip_suffix(['Z', 'z']) {
        return Some((naive, 0));
    }
    let Some(sign_at) = time.rfind(['+', '-']) else {
        return Some((time, 0));
    };
    let (naive, offset) = time.split_at_checked(sign_at)?;
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let digits: String = offset.chars().filter(char::is_ascii_digit).collect();
    let (hours, minutes) = match digits.len() {
        2 => (digits.parse::<i64>().ok()?, 0),
        4 => (digits[..2].parse().ok()?, digits[2..].parse().ok()?),
        _ => return None,
    };
    Some((naive, sign * (hours * 3_600 + minutes * 60)))
}

/// Days since 1970-01-01 for a proleptic-Gregorian civil date.
///
/// Howard Hinnant's `days_from_civil`, written out for the same reason the rest
/// of this module is dependency-free. Total for any `(y, m, d)`; a nonsense day
/// simply lands on a nonsense date rather than panicking.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---------------------------------------------------------------------------
// Channel A — OTLP
// ---------------------------------------------------------------------------

/// A flattened OTLP record, before it becomes an [`Event`].
///
/// Borrowed rather than owned so the decode path allocates once per batch
/// instead of once per attribute.
#[derive(Debug, Clone, Copy)]
pub struct OtlpRecord<'a> {
    /// The event name from the `LogRecord` **body** (`claude_code.tool_result`),
    /// with the `claude_code.` prefix already stripped so it can be looked up in
    /// [`polis_events::OTEL_EVENT_NAMES`].
    ///
    /// The OTLP 1.7 `event_name` field is empty on every record, and
    /// `severity_number` is `UNSPECIFIED` on every record — neither can dispatch.
    pub body_name: &'a str,
    /// Flattened attributes, nested kvlists joined with dots, every value
    /// rendered to a string by [`any_value_to_string`].
    pub attributes: &'a [(String, String)],
    /// `trace_id`, present once the beta traces channel is enabled. Enabling
    /// traces retro-fits these onto log records, so a subagent's `tool_result`
    /// carries a different span from the main agent's.
    pub trace_id: Option<&'a str>,
    /// `span_id`, same story.
    pub span_id: Option<&'a str>,
    /// `service.name` from the resource block. Anything other than
    /// `claude-code` is foreign traffic on a loopback port anyone can post to.
    pub service_name: Option<&'a str>,
    /// `service.version` from the resource block — the Claude Code release that
    /// produced the record. Every drift report should carry it (ADR-0035).
    pub service_version: Option<&'a str>,
}

/// A flattened OTLP span, before it becomes an [`Event`].
///
/// Separate from [`OtlpRecord`] because spans arrive on a different service, are
/// **not topologically ordered within a batch**, and carry their identity in
/// span fields rather than in attributes.
#[derive(Debug, Clone, Copy)]
pub struct OtlpSpan<'a> {
    /// The span name, verbatim and unabbreviated — one of
    /// [`polis_events::OTEL_SPAN_NAMES`].
    pub name: &'a str,
    /// 16 hex characters.
    pub span_id: &'a str,
    /// The parent span, absent for a trace root. Batches are not topologically
    /// ordered, so parentage must be resolved lazily rather than at decode time.
    pub parent_span_id: Option<&'a str>,
    /// 32 hex characters. **One per interaction, not one per session**: an
    /// auxiliary request such as session-title generation arrives under its own
    /// `trace_id` with no parent. Group by `session.id`, which is on every span.
    pub trace_id: &'a str,
    /// Flattened attributes, as for [`OtlpRecord`].
    pub attributes: &'a [(String, String)],
    /// Span duration in nanoseconds, from `end_time_unix_nano - start_time_unix_nano`.
    pub duration_nanos: u64,
}

/// Renders any OTLP `AnyValue` variant to a string.
///
/// **Total over every arm**, including the profiling-only
/// `string_value_strindex` and the absent case. Coerce by key, not by variant:
/// see the module docs and ADR-0015.
pub fn any_value_to_string(value: &AnyValue) -> String {
    let mut out = String::new();
    render_value(value, 0, &mut out);
    out
}

/// The recursive half of [`any_value_to_string`].
fn render_value(value: &AnyValue, depth: usize, out: &mut String) {
    use any_value::Value as V;
    let Some(inner) = value.value.as_ref() else {
        // An `AnyValue` with no arm set is the wire's spelling of "empty". It is
        // legal and it is not an error.
        return;
    };
    match inner {
        V::StringValue(s) => out.push_str(s),
        V::BoolValue(b) => out.push_str(if *b { "true" } else { "false" }),
        V::IntValue(i) => out.push_str(&i.to_string()),
        V::DoubleValue(d) => out.push_str(&d.to_string()),
        V::BytesValue(b) => render_bytes(b, out),
        // Profiling-signal only. The spec says to treat it as a non-fatal
        // unexpected field rather than dropping the record.
        V::StringValueStrindex(i) => {
            out.push('#');
            out.push_str(&i.to_string());
        }
        V::ArrayValue(array) if depth < MAX_DEPTH => {
            out.push('[');
            for (i, item) in array.values.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                render_value(item, depth + 1, out);
            }
            out.push(']');
        }
        V::KvlistValue(list) if depth < MAX_DEPTH => {
            out.push('{');
            for (i, kv) in list.values.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&kv.key);
                out.push('=');
                if let Some(v) = kv.value.as_ref() {
                    render_value(v, depth + 1, out);
                }
            }
            out.push('}');
        }
        V::ArrayValue(_) | V::KvlistValue(_) => out.push('…'),
    }
}

/// Lowercase hex, so a byte attribute is at least comparable.
fn render_bytes(bytes: &[u8], out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for b in bytes.iter().take(64) {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    if bytes.len() > 64 {
        out.push('…');
    }
}

/// Flattens an OTLP attribute list, joining nested kvlist keys with dots.
///
/// PII is dropped here, by name, before anything else sees it: every key in
/// [`polis_events::PII_ATTRIBUTES`] is discarded (ADR-0005).
pub fn flatten_attributes(attributes: &[KeyValue], out: &mut Vec<(String, String)>) {
    flatten_into(attributes, "", 0, out);
}

/// The recursive half of [`flatten_attributes`].
fn flatten_into(
    attributes: &[KeyValue],
    prefix: &str,
    depth: usize,
    out: &mut Vec<(String, String)>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    for kv in attributes {
        let key = if prefix.is_empty() {
            kv.key.clone()
        } else {
            format!("{prefix}.{}", kv.key)
        };
        // ADR-0005: `user.email` is a real address and it rides on every single
        // event. Drop by name, here, before anything can store it.
        if is_pii_attribute(&key) {
            continue;
        }
        if let Some(any_value::Value::KvlistValue(list)) =
            kv.value.as_ref().and_then(|v| v.value.as_ref())
        {
            flatten_into(&list.values, &key, depth + 1, out);
        } else {
            let rendered = kv
                .value
                .as_ref()
                .map(any_value_to_string)
                .unwrap_or_default();
            out.push((key, rendered));
        }
    }
}

/// Recovers file paths from a `tool_input` / `tool_parameters` JSON string.
///
/// Searches `file_path`, `notebook_path`, `path` and `target_file`. Combined
/// with the finding that the `FileChanged` hook cannot attribute, this is the
/// only reliable path-to-agent link on the OTel channel — load-bearing for
/// territory inference, not a detail.
///
/// Values inside `tool_input` are truncated per-value at 128 characters with a
/// `…[N chars]` marker (the docs say 512; observed is 128). `file_path` is
/// unaffected because it is the first and shortest key, but nothing else here
/// may be assumed complete, so a value carrying the marker is skipped rather
/// than keyed on. **JSON-decode, never string-slice**: on Windows the
/// separators are escaped.
pub fn tool_input_paths(tool_input_json: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(value) = serde_json::from_str::<Value>(tool_input_json) else {
        return out;
    };
    collect_paths(&value, 0, &mut out);
    out
}

/// The recursive half of [`tool_input_paths`].
fn collect_paths(value: &Value, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                if PATH_KEYS.contains(&key.as_str()) {
                    if let Some(path) = item.as_str() {
                        // A truncated value is not a path.
                        if !path.contains('…') && !out.iter().any(|p| p == path) {
                            out.push(path.to_owned());
                        }
                    }
                }
                collect_paths(item, depth + 1, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_paths(item, depth + 1, out);
            }
        }
        _ => {}
    }
}

/// Builds the channel-independent envelope from a flattened attribute list.
fn envelope(channel: Channel, attributes: &[(String, String)]) -> EventMeta {
    let mut meta = EventMeta::now(channel);
    if let Some(session) = attr(attributes, "session.id") {
        meta = meta.with_session(SessionId::new(session));
    }
    // ADR-0030: `agent_id` **presence** is the sole worker discriminator.
    // `agent_type` is set on a main agent too, by `claude --agent foo`.
    meta.worker = attr(attributes, "agent_id").map(WorkerId::new);
    meta.agent_type = attr(attributes, "agent_type").map(AgentType::new);
    meta.prompt = attr(attributes, "prompt.id").map(PromptId::new);
    meta.sequence = attr_u64(attributes, "event.sequence");
    meta
}

/// Converts one decoded OTLP log record into a bus event.
///
/// Returns `None` for [`polis_events::REFUSED_EVENT_NAMES`] and for anything
/// whose `service.name` is not `claude-code` — a cheap filter against foreign
/// OTLP traffic on a loopback port that anyone can post to.
///
/// A `body_name` that is not in [`polis_events::OTEL_EVENT_NAMES`] yields
/// [`OtelEvent::Unknown`] rather than `None`, exactly as that table's
/// documentation specifies. The caller should push a companion [`drift`] event
/// whenever it sees one; dropping the record silently is what PRD §17 exists to
/// prevent.
pub fn otlp_record(record: &OtlpRecord<'_>, mapper: &PathMapper) -> Option<Event> {
    if record
        .service_name
        .is_some_and(|name| name != CLAUDE_CODE_SERVICE)
    {
        return None;
    }
    if is_refused_event(record.body_name) {
        return None;
    }
    let meta = envelope(Channel::Otel, record.attributes);
    let body = otel_body(record.body_name, record.attributes, mapper);
    Some(Event::new(meta, Payload::Otel(Box::new(body))))
}

/// Dispatches a `LogRecord` body name onto its [`OtelEvent`] variant.
fn otel_body(name: &str, a: &[(String, String)], mapper: &PathMapper) -> OtelEvent {
    match name {
        "user_prompt" => OtelEvent::UserPrompt {
            length: attr_u64(a, "prompt_length"),
        },
        "assistant_response" => OtelEvent::AssistantResponse,
        "api_request" => OtelEvent::ApiRequest {
            model: attr_owned(a, "model"),
            query_source: attr_owned(a, "query_source"),
            duration_ms: attr_f64(a, "duration_ms"),
        },
        "api_error" => OtelEvent::ApiError {
            error_type: attr_owned(a, "error_type"),
        },
        "api_refusal" => OtelEvent::ApiRefusal,
        "api_retries_exhausted" => OtelEvent::ApiRetriesExhausted,
        "tool_decision" => OtelEvent::ToolDecision {
            tool: tool_kind(attr(a, "tool_name").unwrap_or_default()),
            tool_use_id: attr(a, "tool_use_id").map(ToolUseId::new),
            decision: attr_owned(a, "decision"),
            source: attr_owned(a, "source"),
        },
        "tool_result" => OtelEvent::ToolResult(Box::new(tool_call(a, mapper))),
        "subagent_completed" => OtelEvent::SubagentCompleted {
            total_tool_uses: attr_u64(a, "total_tool_uses"),
            agent_type: attr(a, "agent_type").map(AgentType::new),
        },
        // The wire keys are `from_mode` / `to_mode`, not `permission_mode`
        // (otel-schema.md §5.10, OBSERVED). The variant carries the mode the
        // session ended up in, so `to_mode` is the one to read; the other
        // spellings stay as fallbacks in case a future release renames back.
        "permission_mode_changed" => OtelEvent::PermissionModeChanged {
            mode: attr_owned(a, "to_mode")
                .or_else(|| attr_owned(a, "permission_mode"))
                .or_else(|| attr_owned(a, "mode")),
        },
        "mcp_server_connection" => OtelEvent::McpServerConnection {
            server: attr_owned(a, "server_name").or_else(|| attr_owned(a, "server")),
        },
        // `plugin.name` — dotted, otel-schema.md §5.9 (OBSERVED).
        "plugin_loaded" => OtelEvent::PluginLoaded {
            name: plugin_name(a),
        },
        "plugin_installed" => OtelEvent::PluginInstalled {
            name: plugin_name(a),
        },
        // `skill.name` — dotted, otel-schema.md §5.11 (OBSERVED).
        "skill_activated" => OtelEvent::SkillActivated {
            name: attr_owned(a, "skill.name")
                .or_else(|| attr_owned(a, "skill_name"))
                .or_else(|| attr_owned(a, "name")),
        },
        "at_mention" => OtelEvent::AtMention,
        "compaction" => OtelEvent::Compaction,
        "auth" => OtelEvent::Auth,
        "internal_error" => OtelEvent::InternalError,
        "hook_registered" => OtelEvent::HookRegistered,
        "hook_execution_start" => OtelEvent::HookExecutionStart,
        "hook_execution_complete" => OtelEvent::HookExecutionComplete,
        "hook_plugin_metrics" => OtelEvent::HookPluginMetrics,
        "retention_sweep" => OtelEvent::RetentionSweep,
        "feedback_survey" => OtelEvent::FeedbackSurvey,
        other => OtelEvent::Unknown {
            name: other.to_owned(),
        },
    }
}

/// The plugin name, under whichever spelling arrived.
///
/// `plugin.name` is the observed wire key and rides on `mcp_server_connection`
/// too; the undotted spellings are kept as fallbacks only.
fn plugin_name(a: &[(String, String)]) -> Option<String> {
    attr_owned(a, "plugin.name")
        .or_else(|| attr_owned(a, "plugin_name"))
        .or_else(|| attr_owned(a, "name"))
}

/// Assembles a [`ToolCall`] from whichever attributes arrived.
fn tool_call(a: &[(String, String)], mapper: &PathMapper) -> ToolCall {
    ToolCall {
        tool: tool_kind(attr(a, "tool_name").unwrap_or_default()),
        tool_use_id: attr(a, "tool_use_id")
            .or_else(|| attr(a, "gen_ai.tool.call.id"))
            .map(ToolUseId::new),
        paths: tool_call_paths(a, mapper),
        outcome: outcome(attr(a, "success")),
        duration_ms: attr_f64(a, "duration_ms"),
    }
}

/// Every path a tool call named, preferring the span's clean top-level
/// `file_path` over the JSON inside `tool_input`.
fn tool_call_paths(a: &[(String, String)], mapper: &PathMapper) -> Vec<(WorktreeId, LogicalPath)> {
    let cwd = attr(a, "cwd");
    let mut out = Vec::new();
    let push = |raw: &str, out: &mut Vec<(WorktreeId, LogicalPath)>| {
        // A value carrying the `…[N chars]` truncation marker is not a path.
        // `collect_paths` already applies this guard inside `tool_input`; the
        // top-level `file_path` attribute needs it too, or a 700-character
        // command line arrives as `C:\…\very…[713 chars]` and maps to a real
        // `LogicalPath` — a building that does not exist.
        if raw.contains('…') {
            return;
        }
        if let Some(path) = logical_path_in(mapper, cwd, raw) {
            if !out.contains(&path) {
                out.push(path);
            }
        }
    };
    if let Some(raw) = attr(a, "file_path") {
        push(raw, &mut out);
    }
    for key in ["tool_input", "tool_parameters"] {
        if let Some(json) = attr(a, key) {
            for raw in tool_input_paths(json) {
                push(&raw, &mut out);
            }
        }
    }
    out
}

/// Converts one decoded OTLP span into a bus event.
///
/// All five modelled span names get a variant (ADR-0045); `claude_code.hook`
/// stays unmodelled on purpose and returns `None`.
pub fn otlp_span(span: &OtlpSpan<'_>, mapper: &PathMapper) -> Option<Event> {
    let a = span.attributes;
    let duration =
        attr_f64(a, "duration_ms").or_else(|| Some(nanos_to_millis(span.duration_nanos)));
    let body = match span.name {
        "claude_code.interaction" => OtelEvent::InteractionSpan {
            span_id: Some(span.span_id.to_owned()),
            sequence: attr_u64(a, "interaction.sequence"),
            duration_ms: attr_f64(a, "interaction.duration_ms").or(duration),
            prompt_length: attr_u64(a, "user_prompt_length"),
        },
        "claude_code.llm_request" => OtelEvent::LlmRequestSpan {
            span_id: Some(span.span_id.to_owned()),
            parent_span_id: span.parent_span_id.map(str::to_owned),
            model: attr_owned(a, "model").or_else(|| attr_owned(a, "gen_ai.request.model")),
            context: attr_owned(a, "llm_request.context"),
            duration_ms: duration,
            outcome: outcome(attr(a, "success")),
        },
        "claude_code.tool" => OtelEvent::ToolSpan {
            call: Box::new(tool_call(a, mapper)),
            span_id: Some(span.span_id.to_owned()),
            parent_span_id: span.parent_span_id.map(str::to_owned),
        },
        "claude_code.tool.execution" => OtelEvent::ToolExecutionSpan {
            tool_use_id: span_tool_use_id(a),
            parent_span_id: span.parent_span_id.map(str::to_owned),
            duration_ms: duration,
            outcome: outcome(attr(a, "success")),
        },
        "claude_code.tool.blocked_on_user" => OtelEvent::ToolBlockedOnUserSpan {
            tool_use_id: span_tool_use_id(a),
            parent_span_id: span.parent_span_id.map(str::to_owned),
            duration_ms: duration,
        },
        // ADR-0006: capturing this needs *detailed* beta tracing, which
        // redirects logs and traces to a separate endpoint and would hijack
        // Polis's own export destination. It must stay unmodelled.
        "claude_code.hook" => return None,
        other => OtelEvent::Unknown {
            name: other.to_owned(),
        },
    };
    Some(Event::new(
        envelope(Channel::Otel, a),
        Payload::Otel(Box::new(body)),
    ))
}

/// `tool_use_id` under either of its two spellings on the span side.
fn span_tool_use_id(a: &[(String, String)]) -> Option<ToolUseId> {
    attr(a, "tool_use_id")
        .or_else(|| attr(a, "gen_ai.tool.call.id"))
        .map(ToolUseId::new)
}

/// Nanoseconds to milliseconds without an `as` cast, so no rounding rule is
/// left to the compiler.
///
/// Saturates above ~49 days, which is not a span.
fn nanos_to_millis(nanos: u64) -> f64 {
    let millis = u32::try_from(nanos / 1_000_000).unwrap_or(u32::MAX);
    let remainder = u32::try_from(nanos % 1_000_000).unwrap_or(0);
    f64::from(millis) + f64::from(remainder) / 1e6
}

/// Converts one OTLP metric data point into a bus event.
///
/// **Temporality is read off the wire, never assumed** (ADR-0007), and an
/// `AGGREGATION_TEMPORALITY_UNSPECIFIED` is treated as delta *and* reported as
/// [`polis_events::ControlEvent::SchemaDrift`]. Reading a delta stream as
/// cumulative is a silent failure: every counter collapses to "the last export
/// interval" and still looks plausible.
pub fn otlp_metric(metric: &Metric, out: &mut Vec<Event>) -> usize {
    let before = out.len();
    let name = metric_name(&metric.name);
    let (points, temporality) = match metric.data.as_ref() {
        Some(metric::Data::Sum(sum)) => (
            &sum.data_points,
            temporality_from(sum.aggregation_temporality),
        ),
        // Claude Code's eight counters are all Sums. A Gauge carries no
        // temporality field at all, which is itself worth reporting.
        Some(metric::Data::Gauge(gauge)) => (&gauge.data_points, Temporality::Unspecified),
        _ => return 0,
    };
    if temporality == Temporality::Unspecified {
        out.push(drift(
            Channel::Otel,
            None,
            format!(
                "metric `{}` declared no aggregation temporality",
                metric.name
            ),
        ));
    }
    for point in points {
        let mut attributes = Vec::new();
        flatten_attributes(&point.attributes, &mut attributes);
        let Some(value) = point_value(point.value.as_ref()) else {
            continue;
        };
        let body = OtelMetric {
            name: name.clone(),
            value,
            temporality,
            attributes: attributes
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        };
        out.push(Event::new(
            envelope(Channel::Otel, &attributes),
            Payload::Metric(Box::new(body)),
        ));
    }
    out.len() - before
}

/// The datapoint's value, whichever arm carried it.
#[allow(
    clippy::cast_precision_loss,
    reason = "counter values are far below 2^53; the alternative is refusing a real datapoint"
)]
fn point_value(value: Option<&number_data_point::Value>) -> Option<f64> {
    match value? {
        number_data_point::Value::AsDouble(d) => Some(*d),
        number_data_point::Value::AsInt(i) => Some(*i as f64),
    }
}

/// Maps a wire metric name onto [`MetricName`].
fn metric_name(wire: &str) -> MetricName {
    match wire.strip_prefix("claude_code.").unwrap_or(wire) {
        "session.count" => MetricName::SessionCount,
        "lines_of_code.count" => MetricName::LinesOfCode,
        "pull_request.count" => MetricName::PullRequestCount,
        "commit.count" => MetricName::CommitCount,
        "cost.usage" => MetricName::CostUsage,
        "token.usage" => MetricName::TokenUsage,
        "code_edit_tool.decision" => MetricName::CodeEditToolDecision,
        "active_time.total" => MetricName::ActiveTime,
        _ => MetricName::Other(wire.to_owned()),
    }
}

/// Reads OTLP's aggregation-temporality enum off the wire (ADR-0007).
fn temporality_from(raw: i32) -> Temporality {
    match raw {
        1 => Temporality::Delta,
        2 => Temporality::Cumulative,
        _ => Temporality::Unspecified,
    }
}

// ---------------------------------------------------------------------------
// Channel B — hooks
// ---------------------------------------------------------------------------

/// Converts a decoded hook datagram into a bus event.
///
/// The kind comes from `hook_event_name` in the payload, not from the wire tag
/// (which is a routing hint), and `agent_id` **presence** is the sole worker
/// discriminator — `agent_type` is not, because `claude --agent foo` sets it on
/// a main agent (ADR-0030).
///
/// `tool_input` is **reduced to its path keys** on the way through and the
/// recovered [`LogicalPath`]s are filed under [`NORMALISED_PATHS_KEY`]. A
/// `Write`'s `tool_input` is the whole file and an `Edit`'s is the diff;
/// ADR-0005 says extract `file_path`, then discard the raw JSON, and this is
/// the one place that happens.
pub fn hook_event(mut event: HookEvent, mapper: &PathMapper) -> Event {
    let cwd = event.payload.cwd.clone();
    let paths = hook_paths(&event, mapper, cwd.as_deref());
    if let Some(input) = event.payload.tool_input.take() {
        let reduced = reduce_tool_input(&input);
        event.payload.tool_input = (!reduced.is_null()).then_some(reduced);
    }
    if !paths.is_empty() {
        event
            .payload
            .extra
            .insert(NORMALISED_PATHS_KEY.to_owned(), Value::Array(paths));
    }
    // The envelope itself — and in particular ADR-0030's `agent_id`-presence
    // rule — has exactly one implementation, in `hook_listener`, and it is the
    // one with the test that pins the rule. This function owns the ADR-0005
    // reduction above and nothing else.
    crate::hook_listener::to_event(event)
}

/// The logical paths a hook payload named, as JSON objects.
fn hook_paths(event: &HookEvent, mapper: &PathMapper, cwd: Option<&str>) -> Vec<Value> {
    let Some(input) = event.payload.tool_input.as_ref() else {
        return Vec::new();
    };
    let mut raw = Vec::new();
    collect_paths(input, 0, &mut raw);
    let mut out = Vec::new();
    let mut seen: Vec<(WorktreeId, LogicalPath)> = Vec::new();
    for candidate in raw {
        let Some(entry) = logical_path_in(mapper, cwd, &candidate) else {
            continue;
        };
        if seen.contains(&entry) {
            continue;
        }
        out.push(serde_json::json!({ "worktree": entry.0.0, "path": entry.1.as_str() }));
        seen.push(entry);
    }
    out
}

/// Keeps only the path keys of a `tool_input` object (ADR-0005).
fn reduce_tool_input(input: &Value) -> Value {
    let Some(map) = input.as_object() else {
        // A non-object `tool_input` carries no keys worth keeping and might be
        // an entire file as a bare string.
        return Value::Null;
    };
    let kept = map
        .iter()
        .filter(|(key, value)| PATH_KEYS.contains(&key.as_str()) && value.is_string())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    Value::Object(kept)
}

// ---------------------------------------------------------------------------
// Channel C — filesystem
// ---------------------------------------------------------------------------

/// What a `notify` event kind means once the noise is removed.
///
/// Shared with [`crate::fswatch`]'s debouncer so the two cannot reduce the same
/// kernel event differently — which would show up as one file appearing twice
/// in the city under two different histories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsChange {
    /// A file appeared.
    Created,
    /// A file's contents changed.
    Modified,
    /// A file disappeared.
    Removed,
    /// The `from` half of a rename, arriving on its own.
    RenamedFrom,
    /// The `to` half of a rename, arriving on its own.
    RenamedTo,
    /// A rename reported with both ends in one event.
    RenamedBoth,
}

/// Reduces a `notify` event kind, or `None` when it is noise.
///
/// Noise is `Access` (opening a file is not a mutation), metadata-only
/// `Modify` (a `chmod` does not change the building), and `Any` / `Other`.
pub fn fs_change(kind: notify::EventKind) -> Option<FsChange> {
    use notify::event::{ModifyKind, RenameMode};
    use notify::EventKind as K;
    match kind {
        K::Create(_) => Some(FsChange::Created),
        K::Remove(_) => Some(FsChange::Removed),
        K::Modify(ModifyKind::Name(RenameMode::From)) => Some(FsChange::RenamedFrom),
        K::Modify(ModifyKind::Name(RenameMode::To)) => Some(FsChange::RenamedTo),
        // `Any` on the rename mode means the backend could not tell which half
        // this is; with two paths it is a `Both`, and with one the caller
        // degrades it to a plain modification rather than demolishing a
        // building on a guess.
        K::Modify(ModifyKind::Name(RenameMode::Both | RenameMode::Any | RenameMode::Other)) => {
            Some(FsChange::RenamedBoth)
        }
        K::Modify(ModifyKind::Data(_) | ModifyKind::Any) => Some(FsChange::Modified),
        _ => None,
    }
}

/// Reduces one `notify` event to zero or more bus events.
///
/// Zero, because most `notify` kinds are noise: `Access`, metadata-only
/// `Modify`, and any path excluded by [`crate::fswatch::is_watch_excluded`].
/// More than one, because a single `notify` event can carry several paths.
///
/// A `notify` rescan signal must become
/// [`polis_events::FsEvent::RescanRequired`] rather than being swallowed —
/// ignoring an OS queue overflow silently desynchronises the city from disk.
///
/// The events this produces carry **no attribution and no session**: the
/// filesystem does not know which agent wrote, and `polis_events::FsEvent` has
/// no field in which a guess could hide (ADR-0003, PRD §17).
pub fn fs_event(
    event: &notify::Event,
    worktree: WorktreeId,
    mapper: &PathMapper,
    out: &mut Vec<Event>,
) -> usize {
    let before = out.len();
    if event.need_rescan() {
        out.push(fs_bus_event(FsEvent::RescanRequired));
        return out.len() - before;
    }
    let Some(change) = fs_change(event.kind) else {
        return 0;
    };
    let keys: Vec<(WorktreeId, LogicalPath)> = event
        .paths
        .iter()
        .filter(|p| !is_watch_excluded(p))
        .filter_map(|p| fs_logical(mapper, worktree, p))
        .collect();
    if change == FsChange::RenamedBoth && keys.len() >= 2 {
        out.push(fs_bus_event(FsEvent::Renamed {
            from: keys[0].clone(),
            to: keys[1].clone(),
        }));
        return out.len() - before;
    }
    for path in keys {
        out.push(fs_bus_event(single_fs_event(change, path)));
    }
    out.len() - before
}

/// Maps a watched path onto its logical key.
///
/// The [`PathMapper`] is authoritative — it matches roots longest-first, so it
/// is the half that can tell a worktree from the repository it lives inside.
/// The watcher's own `worktree` is the fallback for the one case the mapper
/// cannot see: a root registered with the watcher but not yet with the mapper
/// resolves to [`WorktreeId::PRIMARY`], and the watcher knows better.
fn fs_logical(
    mapper: &PathMapper,
    worktree: WorktreeId,
    path: &Path,
) -> Option<(WorktreeId, LogicalPath)> {
    let (resolved, logical) = mapper.to_logical(path)?;
    let id = if resolved.is_primary() {
        worktree
    } else {
        resolved
    };
    Some((id, logical))
}

/// One path plus one change becomes one [`FsEvent`].
///
/// Shared with [`crate::fswatch`]'s debouncer, which flushes the same reduction
/// after coalescing. Two copies of this table is how one file ends up in the
/// city twice with two different histories.
pub(crate) fn single_fs_event(change: FsChange, path: (WorktreeId, LogicalPath)) -> FsEvent {
    match change {
        FsChange::Created | FsChange::RenamedTo => FsEvent::Created { path },
        FsChange::Removed | FsChange::RenamedFrom => FsEvent::Removed { path },
        // A `Both` that arrived with fewer than two usable paths: the file was
        // touched, and that is all that can honestly be said.
        FsChange::Modified | FsChange::RenamedBoth => FsEvent::Modified { path },
    }
}

/// Wraps an [`FsEvent`] in an envelope with no identity fields at all.
pub(crate) fn fs_bus_event(event: FsEvent) -> Event {
    Event::new(EventMeta::now(Channel::Fs), Payload::Fs(event))
}

// ---------------------------------------------------------------------------
// Channel D — transcripts
// ---------------------------------------------------------------------------

/// Parses one JSONL line into a bus event.
///
/// `byte_offset` is the record's order (ADR-0014) and is carried through to
/// [`polis_events::TranscriptEvent::byte_offset`]; the record's own `timestamp`
/// is display-only, because 20% of files contain a backwards step and one
/// observed jump was 60 seconds.
///
/// **A malformed line yields `None`, never an error that aborts the file**
/// (PRD §4.4). This is the function PRD §16's fuzz target drives.
///
/// # One record model, not two
///
/// This delegates to [`crate::transcript::transcript_line`], which owns the
/// serde model of all 19 record types, the `scrub_pii` pass and the
/// long-string elision. A second, simpler copy lived here until integration:
/// both compiled, both passed their own tests, and only one of them stripped
/// PII — exactly ADR-0035's invisible-at-runtime trap. The declaration stays so
/// all four channels are reachable through one module.
pub fn transcript_line(
    line: &str,
    source: &TranscriptSource,
    byte_offset: u64,
    mapper: &PathMapper,
) -> Option<Event> {
    crate::transcript::transcript_line(line, source, byte_offset, mapper)
}

/// Classifies a transcript path into its [`TranscriptSource`].
///
/// The `wf_<runId>` segment must be preserved: 503 of 643 subagent transcripts
/// are workflow ones and that run id is their *only* parent link (ADR-0013,
/// ADR-0047). Depth is variable — glob for `agent-*.jsonl`, never assume it.
///
/// Delegates to [`crate::transcript::transcript_source`] for the same reason
/// [`transcript_line`] does.
pub fn transcript_source(path: &Path) -> Option<TranscriptSource> {
    crate::transcript::transcript_source(path)
}

#[cfg(test)]
mod tests {
    use polis_events::{EventKind, HookPayload, TranscriptRecordKind, WallTime};

    use super::*;

    fn mapper() -> PathMapper {
        PathMapper::new(Path::new(if cfg!(windows) { "C:\\repo" } else { "/repo" }))
            .expect("a repo root is mappable")
    }

    fn abs(rel: &str) -> String {
        if cfg!(windows) {
            format!("C:\\repo\\{}", rel.replace('/', "\\"))
        } else {
            format!("/repo/{rel}")
        }
    }

    #[test]
    fn a_path_loses_its_worktree_prefix_and_keeps_its_logical_key() {
        let mut mapper = mapper();
        let wt = WorktreeId(3);
        mapper
            .add_worktree(
                wt,
                Path::new(if cfg!(windows) {
                    "C:\\repo-wt-3"
                } else {
                    "/repo-wt-3"
                }),
            )
            .expect("worktree registers");

        let primary = logical_path(&mapper, &abs("src/auth.ts")).expect("inside the repo");
        let worktree = logical_path(
            &mapper,
            if cfg!(windows) {
                "C:\\repo-wt-3\\src\\auth.ts"
            } else {
                "/repo-wt-3/src/auth.ts"
            },
        )
        .expect("inside the worktree");

        // PRD §7.6: same logical file, two physical places, one layout key.
        assert_eq!(primary.1, worktree.1);
        assert_eq!(primary.1.as_str(), "src/auth.ts");
        assert_eq!(primary.0, WorktreeId::PRIMARY);
        assert_eq!(worktree.0, wt);

        // Outside every root is a normal condition, not an error.
        assert!(logical_path(&mapper, "/elsewhere/x.rs").is_none());
    }

    /// ADR-0032 / `hooks-schema.md` §9.7: on a Windows box without Git Bash the
    /// `Bash` tool is never registered, so a shell check written against it
    /// matches nothing at all.
    #[test]
    fn both_shells_map_to_shells_and_weigh_the_same() {
        assert_eq!(tool_kind("Bash"), ToolKind::Bash);
        assert_eq!(tool_kind("PowerShell"), ToolKind::PowerShell);
        assert!(is_shell_tool("Bash"));
        assert!(is_shell_tool("PowerShell"));
        assert!(!is_shell_tool("Edit"));
        assert!(
            (tool_kind("PowerShell").evidence_weight() - tool_kind("Bash").evidence_weight()).abs()
                < f32::EPSILON
        );
        // The two tools that do not exist must not be special-cased into one
        // that does (ADR-0031).
        assert!(!tool_kind("Task").spawns_worker());
        assert!(!tool_kind("MultiEdit").is_mutating());
        assert!(matches!(tool_kind("mcp__srv__do"), ToolKind::Mcp(_)));
    }

    #[test]
    fn outcome_reads_the_string_spellings_the_wire_actually_uses() {
        assert_eq!(outcome(Some("true")), Outcome::Done);
        assert_eq!(outcome(Some("false")), Outcome::Failed);
        assert_eq!(outcome(Some("TRUE")), Outcome::Done);
        // Absent is pending, not failed: `tool_result` may simply not have
        // arrived yet.
        assert_eq!(outcome(None), Outcome::Pending);
        assert_eq!(outcome(Some("weird")), Outcome::Pending);
    }

    #[test]
    fn scalars_are_coerced_by_key_not_by_wire_variant() {
        let attrs = vec![
            ("duration_ms".to_owned(), "890".to_owned()),
            ("cost_usd".to_owned(), "0.000945".to_owned()),
            ("safe_mode".to_owned(), "false".to_owned()),
            ("has_hooks".to_owned(), "true".to_owned()),
            ("prompt_length".to_owned(), "42.0".to_owned()),
            ("ragged".to_owned(), "42.5".to_owned()),
        ];
        assert_eq!(attr_f64(&attrs, "duration_ms"), Some(890.0));
        assert_eq!(attr_u64(&attrs, "duration_ms"), Some(890));
        assert_eq!(attr_bool(&attrs, "safe_mode"), Some(false));
        assert_eq!(attr_bool(&attrs, "has_hooks"), Some(true));
        assert_eq!(attr_u64(&attrs, "prompt_length"), Some(42));
        assert_eq!(attr_u64(&attrs, "ragged"), None, "42.5 is not an integer");
        assert_eq!(attr(&attrs, "absent"), None);
    }

    #[test]
    fn rfc3339_timestamps_parse_without_a_date_crate() {
        let epoch = wall_from_rfc3339("1970-01-01T00:00:00Z").expect("epoch");
        assert_eq!(epoch.unix_millis(), 0);

        let observed = wall_from_rfc3339("2026-09-01T17:38:50.290Z").expect("observed stamp");
        assert_eq!(observed.unix_millis(), 1_788_284_330_290);

        // An offset is applied, not ignored.
        let utc = wall_from_rfc3339("2026-09-01T17:38:50Z").expect("utc");
        let plus2 = wall_from_rfc3339("2026-09-01T19:38:50+02:00").expect("+02:00");
        assert_eq!(utc.unix_millis(), plus2.unix_millis());

        // Short fractions pad to milliseconds.
        assert_eq!(
            wall_from_rfc3339("1970-01-01T00:00:00.5Z")
                .expect("half a second")
                .unix_millis(),
            500
        );
        assert_eq!(
            wall_from_rfc3339("1970-01-01T00:00:00.25Z")
                .expect("quarter second")
                .unix_millis(),
            250
        );
        // Pre-epoch is arithmetic, not a panic.
        assert!(
            wall_from_rfc3339("1969-12-31T23:59:59Z")
                .expect("pre-epoch")
                .unix_millis()
                < 0
        );
    }

    #[test]
    fn a_malformed_timestamp_is_none_rather_than_a_panic() {
        for bad in [
            "",
            "not a date",
            "2026-13-01T00:00:00Z",
            "2026-09-01T25:00:00Z",
            "2026-09-01",
            "2026-09-01T00:00Z",
            "2026-09-01T00:00:00.Z",
            "----------",
            "9999999999999999999-01-01T00:00:00Z",
        ] {
            assert!(wall_from_rfc3339(bad).is_none(), "{bad} parsed");
        }
        // WallTime arithmetic stays total either way.
        assert_eq!(WallTime::from_unix_millis(0).unix_millis(), 0);
    }

    #[test]
    fn pii_is_dropped_by_name_before_anything_can_store_it() {
        let attrs = vec![
            KeyValue {
                key: "user.email".to_owned(),
                value: Some(string_value("a@example.com")),
                ..KeyValue::default()
            },
            KeyValue {
                key: "session.id".to_owned(),
                value: Some(string_value("s-1")),
                ..KeyValue::default()
            },
        ];
        let mut out = Vec::new();
        flatten_attributes(&attrs, &mut out);
        assert_eq!(out, vec![("session.id".to_owned(), "s-1".to_owned())]);
        assert!(is_pii_attribute("user.email"));
        assert!(is_refused_event("api_request_body"));
    }

    fn string_value(s: &str) -> AnyValue {
        AnyValue {
            value: Some(any_value::Value::StringValue(s.to_owned())),
        }
    }

    #[test]
    fn any_value_is_total_over_every_arm_including_the_empty_one() {
        use any_value::Value as V;
        let cases = [
            (V::StringValue("x".into()), "x"),
            (V::BoolValue(true), "true"),
            (V::IntValue(-3), "-3"),
            (V::DoubleValue(1.5), "1.5"),
            (V::BytesValue(vec![0xde, 0xad]), "dead"),
            (V::StringValueStrindex(7), "#7"),
        ];
        for (value, expected) in cases {
            assert_eq!(
                any_value_to_string(&AnyValue { value: Some(value) }),
                expected
            );
        }
        assert_eq!(any_value_to_string(&AnyValue { value: None }), "");
    }

    #[test]
    fn tool_input_paths_are_decoded_not_sliced() {
        // Windows separators are escaped inside the JSON string; slicing would
        // hand back `C:reposrca.rs`.
        let json = r#"{"file_path":"C:\\repo\\src\\a.rs","content":"…[9001 chars]"}"#;
        assert_eq!(tool_input_paths(json), vec!["C:\\repo\\src\\a.rs"]);
        // Nested shapes and every path key.
        let json = r#"{"edits":[{"path":"/repo/b.rs"}],"notebook_path":"/repo/n.ipynb"}"#;
        let found = tool_input_paths(json);
        assert!(found.contains(&"/repo/b.rs".to_owned()));
        assert!(found.contains(&"/repo/n.ipynb".to_owned()));
        // A truncated value is not a path.
        assert!(tool_input_paths(r#"{"path":"/repo/very…[300 chars]"}"#).is_empty());
        // Garbage is empty, never a panic.
        assert!(tool_input_paths("{").is_empty());
        assert!(tool_input_paths("").is_empty());
    }

    #[test]
    fn an_unmodelled_otel_event_becomes_unknown_rather_than_vanishing() {
        let attrs = vec![("session.id".to_owned(), "s-1".to_owned())];
        let record = OtlpRecord {
            body_name: "nucleation",
            attributes: &attrs,
            trace_id: None,
            span_id: None,
            service_name: Some(CLAUDE_CODE_SERVICE),
            service_version: Some("2.1.196"),
        };
        let event = otlp_record(&record, &mapper()).expect("drift is still an event");
        let Payload::Otel(body) = &event.payload else {
            panic!("wrong payload: {:?}", event.payload)
        };
        assert!(matches!(&**body, OtelEvent::Unknown { name } if name == "nucleation"));
        assert_eq!(event.meta.thread.as_ref().unwrap().as_str(), "s-1");
    }

    #[test]
    fn refused_events_and_foreign_traffic_never_become_events() {
        let attrs: Vec<(String, String)> = Vec::new();
        let refused = OtlpRecord {
            body_name: "api_request_body",
            attributes: &attrs,
            trace_id: None,
            span_id: None,
            service_name: Some(CLAUDE_CODE_SERVICE),
            service_version: None,
        };
        assert!(otlp_record(&refused, &mapper()).is_none());

        let foreign = OtlpRecord {
            body_name: "tool_result",
            attributes: &attrs,
            trace_id: None,
            span_id: None,
            service_name: Some("some-other-app"),
            service_version: None,
        };
        assert!(otlp_record(&foreign, &mapper()).is_none());
    }

    #[test]
    fn a_tool_result_carries_its_paths_and_its_string_spelled_success() {
        let attrs = vec![
            ("session.id".to_owned(), "s-1".to_owned()),
            ("tool_name".to_owned(), "Edit".to_owned()),
            ("tool_use_id".to_owned(), "toolu_1".to_owned()),
            ("success".to_owned(), "true".to_owned()),
            ("duration_ms".to_owned(), "12".to_owned()),
            (
                "tool_input".to_owned(),
                format!(r#"{{"file_path":{}}}"#, serde_json::json!(abs("src/a.rs"))),
            ),
        ];
        let record = OtlpRecord {
            body_name: "tool_result",
            attributes: &attrs,
            trace_id: None,
            span_id: None,
            service_name: None,
            service_version: None,
        };
        let event = otlp_record(&record, &mapper()).expect("modelled");
        let Payload::Otel(body) = &event.payload else {
            panic!("wrong payload")
        };
        let OtelEvent::ToolResult(call) = &**body else {
            panic!("wrong variant")
        };
        assert_eq!(call.tool, ToolKind::Edit);
        assert_eq!(call.outcome, Outcome::Done);
        assert_eq!(call.paths.len(), 1);
        assert_eq!(call.paths[0].1.as_str(), "src/a.rs");
    }

    #[test]
    fn the_hook_span_stays_unmodelled_and_the_other_five_do_not() {
        let attrs: Vec<(String, String)> = Vec::new();
        let span = |name: &'static str| OtlpSpan {
            name,
            span_id: "1b69803d0429f2e8",
            parent_span_id: None,
            trace_id: "00000000000000000000000000000000",
            attributes: &attrs,
            duration_nanos: 2_500_000,
        };
        assert!(otlp_span(&span("claude_code.hook"), &mapper()).is_none());
        for name in [
            "claude_code.interaction",
            "claude_code.llm_request",
            "claude_code.tool",
            "claude_code.tool.execution",
            "claude_code.tool.blocked_on_user",
        ] {
            let event = otlp_span(&span(name), &mapper()).expect(name);
            let Payload::Otel(body) = &event.payload else {
                panic!("wrong payload")
            };
            assert!(
                !matches!(&**body, OtelEvent::Unknown { .. }),
                "{name} degraded to Unknown"
            );
        }
    }

    /// ADR-0005: a `Write`'s `tool_input` is the entire file and an `Edit`'s is
    /// the diff. Neither may survive normalisation.
    #[test]
    fn a_hook_payload_keeps_its_paths_and_loses_its_tool_input_body() {
        let payload = HookPayload {
            session_id: Some(SessionId::new("s-1")),
            hook_event_name: Some("PreToolUse".to_owned()),
            tool_name: Some("Write".to_owned()),
            tool_input: Some(serde_json::json!({
                "file_path": abs("src/a.rs"),
                "content": "every byte of the file"
            })),
            ..HookPayload::default()
        };
        let event = hook_event(
            HookEvent {
                kind: EventKind::PreToolUse,
                truncated: false,
                payload,
            },
            &mapper(),
        );
        let Payload::Hook(hook) = &event.payload else {
            panic!("wrong payload")
        };
        let input = hook.payload.tool_input.as_ref().expect("input survives");
        assert!(input.get("file_path").is_some(), "the path is kept");
        assert!(
            input.get("content").is_none(),
            "the file body must not survive ingest"
        );
        let paths = hook.payload.extra[NORMALISED_PATHS_KEY]
            .as_array()
            .expect("normalised paths");
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0]["path"], "src/a.rs");
        assert_eq!(event.meta.thread.as_ref().unwrap().as_str(), "s-1");
        assert!(!event.meta.is_worker(), "no agent_id means main agent");
    }

    #[test]
    fn transcript_sources_survive_variable_depth() {
        let cases: [(&str, TranscriptSource); 3] = [
            ("/p/projects/x/68160373.jsonl", TranscriptSource::Main),
            (
                "/p/projects/x/68160373/subagents/agent-a106.jsonl",
                TranscriptSource::Subagent {
                    agent: WorkerId::new("a106"),
                    workflow_run: None,
                },
            ),
            (
                "/p/projects/x/68160373/subagents/workflows/wf_01JQ/agent-a96c.jsonl",
                TranscriptSource::Subagent {
                    agent: WorkerId::new("a96c"),
                    workflow_run: Some("01JQ".to_owned()),
                },
            ),
        ];
        for (path, expected) in cases {
            assert_eq!(transcript_source(Path::new(path)), Some(expected), "{path}");
        }
        assert_eq!(
            transcript_source(Path::new(
                "/p/projects/x/s/subagents/workflows/wf_01JQ/journal.jsonl"
            )),
            Some(TranscriptSource::WorkflowJournal {
                run: "01JQ".to_owned()
            })
        );
        assert!(transcript_source(Path::new("/p/notes.txt")).is_none());
    }

    #[test]
    fn one_bad_transcript_line_is_a_skipped_line_not_an_aborted_file() {
        let m = mapper();
        for bad in ["", "   ", "{", "null", "[]", "\u{feff}{}", "{\"type\":"] {
            assert!(
                transcript_line(bad, &TranscriptSource::Main, 0, &m).is_none(),
                "{bad:?} was not skipped"
            );
        }
        let good = r#"{"type":"assistant","sessionId":"s-1","uuid":"u1"}"#;
        let event = transcript_line(good, &TranscriptSource::Main, 4_096, &m).expect("parses");
        let Payload::Transcript(body) = &event.payload else {
            panic!("wrong payload")
        };
        assert_eq!(body.kind, TranscriptRecordKind::Assistant);
        assert_eq!(body.byte_offset, 4_096, "order is byte offset, ADR-0014");

        // A record type from a future release is drift, never a parse failure.
        let future = r#"{"type":"invented-in-2-2-0"}"#;
        let event = transcript_line(future, &TranscriptSource::Main, 0, &m).expect("parses");
        let Payload::Transcript(body) = &event.payload else {
            panic!("wrong payload")
        };
        assert_eq!(body.kind, TranscriptRecordKind::Unknown);
    }

    #[test]
    fn notify_noise_reduces_to_nothing_and_mutations_reduce_to_something() {
        use notify::event::{AccessKind, DataChange, MetadataKind, ModifyKind};
        use notify::EventKind as K;
        assert_eq!(fs_change(K::Access(AccessKind::Any)), None);
        assert_eq!(
            fs_change(K::Modify(ModifyKind::Metadata(MetadataKind::Permissions))),
            None,
            "a chmod does not change the building"
        );
        assert_eq!(
            fs_change(K::Modify(ModifyKind::Data(DataChange::Content))),
            Some(FsChange::Modified)
        );
        assert_eq!(fs_change(K::Any), None);
    }

    /// PRD §17 and ADR-0003: nothing on this channel may look attributed.
    #[test]
    fn filesystem_events_reach_the_bus_with_no_attribution_at_all() {
        let event = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Content,
        )))
        .add_path(std::path::PathBuf::from(abs("src/a.rs")));
        let mut out = Vec::new();
        assert_eq!(
            fs_event(&event, WorktreeId::PRIMARY, &mapper(), &mut out),
            1
        );
        assert!(matches!(
            &out[0].payload,
            Payload::Fs(FsEvent::Modified { .. })
        ));
        assert!(out[0].meta.session.is_none());
        assert!(out[0].meta.thread.is_none());
        assert!(out[0].meta.worker.is_none());
        assert!(out[0].meta.prompt.is_none());
    }

    #[test]
    fn a_rescan_flag_is_never_swallowed() {
        let event =
            notify::Event::new(notify::EventKind::Any).set_flag(notify::event::Flag::Rescan);
        let mut out = Vec::new();
        assert_eq!(
            fs_event(&event, WorktreeId::PRIMARY, &mapper(), &mut out),
            1
        );
        assert!(matches!(
            &out[0].payload,
            Payload::Fs(FsEvent::RescanRequired)
        ));
    }
}
