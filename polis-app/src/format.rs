//! Rendering one normalised [`Event`] as a line of text (PRD §15 M0).
//!
//! > `polis tail` prints a normalized event stream to stdout. No graphics.
//!
//! This is the only place in Polis that turns an [`Event`] into prose, and it is
//! shared by `polis tail` and `polis replay` so the two cannot disagree about
//! what an event *is*.
//!
//! # Two rules
//!
//! 1. **Nothing here invents an identity.** A `None` worker prints as `-`,
//!    never as "main agent", because on a session with the beta traces channel
//!    off *every* tool call has `None` there and calling that "main agent" is
//!    the exact silent lie ADR-0006 forbids.
//! 2. **Every `#[non_exhaustive]` match has a wildcard arm that prints the
//!    serde label** rather than dropping the event. A Claude Code release adding
//!    an event name must show up as a line saying so, not as a missing line.

use std::time::Duration;

use polis_events::{
    Channel, ControlEvent, Event, FsEvent, LogicalPath, MetricName, OtelEvent, Outcome, Payload,
    PromptId, SessionId, ToolCall, TranscriptSource, WallTime, WorkerId, WorktreeId,
};
use serde::Serialize;

/// One event, decomposed into the four columns `polis tail` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Described {
    /// Which channel produced it.
    pub channel: Channel,
    /// The event's own name, in its wire spelling where it has one.
    pub kind: String,
    /// Session / worker / prompt, already abbreviated.
    pub ident: String,
    /// Everything else worth a glance: tool, outcome, paths, counts.
    pub detail: String,
}

/// Decomposes an event into printable columns.
pub fn describe(event: &Event) -> Described {
    let (kind, detail) = match &event.payload {
        Payload::Otel(otel) => describe_otel(otel),
        Payload::Metric(metric) => (format!("metric.{}", metric_label(&metric.name)), {
            let attributes = attribute_tail(&metric.attributes);
            let mut detail = format!("{:.3} {:?}", metric.value, metric.temporality);
            push_field(&mut detail, &attributes);
            detail
        }),
        Payload::Hook(hook) => {
            let mut detail = String::new();
            if let Some(tool) = &hook.payload.tool_name {
                detail.push_str(tool);
            }
            let paths = hook_paths(hook);
            if !paths.is_empty() {
                push_field(&mut detail, &paths.join(" "));
            }
            if hook.truncated {
                push_field(&mut detail, "[truncated]");
            }
            (format!("hook.{}", hook.kind.name()), detail)
        }
        Payload::Fs(fs) => describe_fs(fs),
        Payload::Transcript(t) => (
            format!("transcript.{}", variant_label(&t.kind)),
            format!("{} @{}", source_label(&t.source), t.byte_offset),
        ),
        Payload::Control(control) => describe_control(control),
        other => (variant_label(other), String::new()),
    };
    Described {
        channel: event.meta.channel,
        kind,
        ident: ident(event),
        detail,
    }
}

/// Renders a described event as one aligned line.
///
/// `seconds` is the monotonic offset from the start of the stream, never a wall
/// clock: it is the only clock that cannot step backwards, and it is what the
/// ±2 s attribution window is anchored on (ADR-0014).
pub fn line(seconds: f64, described: &Described) -> String {
    let mut out = format!(
        "{seconds:>9.3}  {:<10} {:<37} {:<26}",
        described.channel.to_string(),
        described.kind,
        described.ident
    );
    if !described.detail.is_empty() {
        out.push(' ');
        out.push_str(&described.detail);
    }
    // Trailing padding on an empty detail column is noise in a terminal and
    // noise in a diff.
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// The header the aligned form is read under.
pub fn header() -> String {
    format!(
        "{:>9}  {:<10} {:<37} {:<26} {}",
        "t+s", "channel", "event", "session/worker/prompt", "detail"
    )
}

fn describe_otel(otel: &OtelEvent) -> (String, String) {
    match otel {
        OtelEvent::UserPrompt { length } => (
            "user_prompt".to_owned(),
            length.map_or_else(String::new, |n| format!("{n} chars")),
        ),
        OtelEvent::AssistantResponse => ("assistant_response".to_owned(), String::new()),
        OtelEvent::ApiRequest {
            model,
            query_source,
            duration_ms,
        } => {
            let mut detail = model.clone().unwrap_or_default();
            if let Some(source) = query_source {
                push_field(&mut detail, source);
            }
            push_duration(&mut detail, *duration_ms);
            ("api_request".to_owned(), detail)
        }
        OtelEvent::ApiError { error_type } => (
            "api_error".to_owned(),
            error_type.clone().unwrap_or_default(),
        ),
        OtelEvent::ToolDecision {
            tool,
            decision,
            source,
            ..
        } => {
            let mut detail = tool.name().to_owned();
            push_field(&mut detail, decision.as_deref().unwrap_or("?"));
            if let Some(source) = source {
                push_field(&mut detail, source);
            }
            ("tool_decision".to_owned(), detail)
        }
        OtelEvent::ToolResult(call) => ("tool_result".to_owned(), tool_detail(call)),
        OtelEvent::SubagentCompleted {
            total_tool_uses,
            agent_type,
        } => {
            let mut detail = agent_type
                .as_ref()
                .map(|a| a.as_str().to_owned())
                .unwrap_or_default();
            if let Some(n) = total_tool_uses {
                push_field(&mut detail, &format!("{n} tool uses"));
            }
            ("subagent_completed".to_owned(), detail)
        }
        OtelEvent::PermissionModeChanged { mode } => (
            "permission_mode_changed".to_owned(),
            mode.clone().unwrap_or_default(),
        ),
        OtelEvent::McpServerConnection { server } => (
            "mcp_server_connection".to_owned(),
            server.clone().unwrap_or_default(),
        ),
        OtelEvent::PluginLoaded { name } => {
            ("plugin_loaded".to_owned(), name.clone().unwrap_or_default())
        }
        OtelEvent::PluginInstalled { name } => (
            "plugin_installed".to_owned(),
            name.clone().unwrap_or_default(),
        ),
        OtelEvent::SkillActivated { name } => (
            "skill_activated".to_owned(),
            name.clone().unwrap_or_default(),
        ),
        // The markers: no payload worth a column, but their wire names are
        // what an operator greps `docs/verified/otel-schema.md` §5.1 for, so
        // they are spelled out rather than left to the CamelCase fallback.
        OtelEvent::ApiRefusal => ("api_refusal".to_owned(), String::new()),
        OtelEvent::ApiRetriesExhausted => ("api_retries_exhausted".to_owned(), String::new()),
        OtelEvent::AtMention => ("at_mention".to_owned(), String::new()),
        OtelEvent::Compaction => ("compaction".to_owned(), String::new()),
        OtelEvent::Auth => ("auth".to_owned(), String::new()),
        OtelEvent::InternalError => ("internal_error".to_owned(), String::new()),
        OtelEvent::HookRegistered => ("hook_registered".to_owned(), String::new()),
        OtelEvent::HookExecutionStart => ("hook_execution_start".to_owned(), String::new()),
        OtelEvent::HookExecutionComplete => ("hook_execution_complete".to_owned(), String::new()),
        OtelEvent::HookPluginMetrics => ("hook_plugin_metrics".to_owned(), String::new()),
        OtelEvent::RetentionSweep => ("retention_sweep".to_owned(), String::new()),
        OtelEvent::FeedbackSurvey => ("feedback_survey".to_owned(), String::new()),
        OtelEvent::Unknown { name } => ("unknown".to_owned(), name.clone()),
        // Split out so this function stays readable: the five span names are a
        // separate signal on a separate OTLP service.
        span => describe_span(span),
    }
}

/// The five `claude_code.*` spans (ADR-0045).
///
/// These are the only place a subagent's work is distinguishable from the main
/// agent's: `agent_id` rides on the span and on nothing else (ADR-0006).
fn describe_span(span: &OtelEvent) -> (String, String) {
    match span {
        OtelEvent::ToolSpan { call, span_id, .. } => {
            let mut detail = tool_detail(call);
            push_field(&mut detail, &format!("span={}", short(span_id.as_deref())));
            ("span.claude_code.tool".to_owned(), detail)
        }
        OtelEvent::InteractionSpan {
            sequence,
            duration_ms,
            prompt_length,
            ..
        } => {
            let mut detail = sequence.map_or_else(String::new, |s| format!("turn {s}"));
            if let Some(n) = prompt_length {
                push_field(&mut detail, &format!("{n} chars"));
            }
            push_duration(&mut detail, *duration_ms);
            ("span.claude_code.interaction".to_owned(), detail)
        }
        OtelEvent::LlmRequestSpan {
            model,
            context,
            duration_ms,
            outcome,
            ..
        } => {
            let mut detail = model.clone().unwrap_or_default();
            if let Some(context) = context {
                push_field(&mut detail, context);
            }
            push_field(&mut detail, outcome_label(*outcome));
            push_duration(&mut detail, *duration_ms);
            ("span.claude_code.llm_request".to_owned(), detail)
        }
        OtelEvent::ToolExecutionSpan {
            duration_ms,
            outcome,
            ..
        } => {
            let mut detail = outcome_label(*outcome).to_owned();
            push_duration(&mut detail, *duration_ms);
            ("span.claude_code.tool.execution".to_owned(), detail)
        }
        OtelEvent::ToolBlockedOnUserSpan { duration_ms, .. } => {
            let mut detail = String::new();
            push_duration(&mut detail, *duration_ms);
            ("span.claude_code.tool.blocked_on_user".to_owned(), detail)
        }
        // Every other modelled name is a bare marker with no payload worth a
        // column; the serde label is its wire name in CamelCase. This arm is
        // also what keeps a variant a future release adds visible in the stream
        // rather than silently absent.
        other => (variant_label(other), String::new()),
    }
}

fn describe_fs(fs: &FsEvent) -> (String, String) {
    match fs {
        FsEvent::Created { path } => ("fs.created".to_owned(), path_label(path)),
        FsEvent::Modified { path } => ("fs.modified".to_owned(), path_label(path)),
        FsEvent::Removed { path } => ("fs.removed".to_owned(), path_label(path)),
        FsEvent::Renamed { from, to } => (
            "fs.renamed".to_owned(),
            format!("{} -> {}", path_label(from), path_label(to)),
        ),
        FsEvent::RescanRequired => (
            "fs.rescan".to_owned(),
            "the OS event queue overflowed; the tree must be re-scanned".to_owned(),
        ),
        other => (variant_label(other), String::new()),
    }
}

fn describe_control(control: &ControlEvent) -> (String, String) {
    match control {
        ControlEvent::ChannelDegraded { channel, reason } => (
            "control.degraded".to_owned(),
            format!("{channel}: {reason}"),
        ),
        ControlEvent::EventsDropped { count, channel } => (
            "control.dropped".to_owned(),
            format!("{count} from {channel}"),
        ),
        ControlEvent::SequenceGap {
            session,
            expected,
            got,
        } => (
            "control.sequence_gap".to_owned(),
            format!(
                "session {} expected {expected}, got {got}",
                short(Some(session.as_str()))
            ),
        ),
        ControlEvent::SchemaDrift {
            channel,
            producer_version,
            detail,
        } => (
            "control.drift".to_owned(),
            format!(
                "{channel}{}: {detail}",
                producer_version
                    .as_deref()
                    .map_or_else(String::new, |v| format!(" (v{v})"))
            ),
        ),
        ControlEvent::Shutdown => ("control.shutdown".to_owned(), String::new()),
        other => (variant_label(other), String::new()),
    }
}

fn tool_detail(call: &ToolCall) -> String {
    let mut detail = call.tool.name().to_owned();
    push_field(&mut detail, outcome_label(call.outcome));
    push_duration(&mut detail, call.duration_ms);
    for path in &call.paths {
        push_field(&mut detail, &path_label(path));
    }
    detail
}

fn outcome_label(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Done => "ok",
        Outcome::Failed => "FAIL",
        // Neutral, and the normal state: `tool_result` may be dropped and
        // `toolUseResult` is missing on 65% of subagent tool results. Printing
        // it as a failure would make a healthy session look broken.
        Outcome::Pending => "pending",
    }
}

/// The logical paths `normalize::hook_event` recovered before it discarded the
/// raw `tool_input` (ADR-0005).
fn hook_paths(hook: &polis_events::HookEvent) -> Vec<String> {
    hook.payload
        .extra
        .get(polis_ingest::normalize::NORMALISED_PATHS_KEY)
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let path = entry.get("path")?.as_str()?;
                    let worktree = entry.get("worktree").and_then(serde_json::Value::as_u64);
                    Some(match worktree {
                        Some(0) | None => path.to_owned(),
                        Some(id) => format!("wt{id}:{path}"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn path_label(path: &(WorktreeId, LogicalPath)) -> String {
    if path.0.is_primary() {
        path.1.as_str().to_owned()
    } else {
        // A worktree is a *different physical place for the same logical file*
        // (PRD §7.6), so the id has to be visible or two agents on two branches
        // read as one.
        format!("wt{}:{}", path.0 .0, path.1.as_str())
    }
}

fn source_label(source: &TranscriptSource) -> String {
    match source {
        TranscriptSource::Main => "main".to_owned(),
        TranscriptSource::Subagent {
            agent,
            workflow_run,
        } => match workflow_run {
            Some(run) => format!("agent {} (wf_{run})", short(Some(agent.as_str()))),
            None => format!("agent {}", short(Some(agent.as_str()))),
        },
        TranscriptSource::WorkflowJournal { run } => format!("journal wf_{run}"),
    }
}

fn metric_label(name: &MetricName) -> String {
    match name {
        MetricName::SessionCount => "session.count".to_owned(),
        MetricName::LinesOfCode => "lines_of_code.count".to_owned(),
        MetricName::PullRequestCount => "pull_request.count".to_owned(),
        MetricName::CommitCount => "commit.count".to_owned(),
        MetricName::CostUsage => "cost.usage".to_owned(),
        MetricName::TokenUsage => "token.usage".to_owned(),
        MetricName::CodeEditToolDecision => "code_edit_tool.decision".to_owned(),
        MetricName::ActiveTime => "active_time.total".to_owned(),
        MetricName::Other(other) => other.clone(),
    }
}

/// The two or three metric attributes worth a column, in a fixed order.
///
/// A `BTreeMap`-ordered dump of every attribute would be a hundred characters
/// per data point; these are the ones that change what a value *means*.
fn attribute_tail(attributes: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut out = String::new();
    for key in ["type", "model", "decision", "language", "tool"] {
        if let Some(value) = attributes.get(key).map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }) {
            push_field(&mut out, &format!("{key}={value}"));
        }
    }
    out
}

/// `session/worker/prompt`, abbreviated, with `-` for absent.
///
/// Absent is meaningful and must be visible: a `None` worker on every line means
/// the beta traces channel is off and no subagent can be told from the main
/// agent (ADR-0006).
fn ident(event: &Event) -> String {
    format!(
        "{}/{}/{}",
        short(event.meta.session.as_ref().map(SessionId::as_str)),
        short(event.meta.worker.as_ref().map(WorkerId::as_str)),
        short(event.meta.prompt.as_ref().map(PromptId::as_str))
    )
}

/// The first eight characters of an id, or `-`.
///
/// Eight, because a session id is a UUID and its first block is already unique
/// among the handful of sessions one operator has open.
fn short(id: Option<&str>) -> String {
    match id {
        None | Some("") => "-".to_owned(),
        // Char-wise, not byte-wise: an id is ASCII today and slicing bytes would
        // panic the whole stream the day one is not.
        Some(id) => id.chars().take(8).collect(),
    }
}

fn push_field(out: &mut String, field: &str) {
    if field.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(field);
}

fn push_duration(out: &mut String, duration_ms: Option<f64>) {
    if let Some(ms) = duration_ms {
        push_field(out, &format!("{ms:.0}ms"));
    }
}

/// The serde label of an enum variant, for the wildcard arms.
///
/// Serde's external tagging writes `{"NewVariant": …}` for a data variant and
/// `"NewVariant"` for a unit one, so the first object key (or the string) is the
/// variant name. This is what keeps a Claude Code release that adds a variant
/// visible in the stream instead of silently absent.
fn variant_label<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        Ok(serde_json::Value::Object(map)) => {
            map.keys().next().cloned().unwrap_or_else(|| "?".to_owned())
        }
        _ => "?".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Clocks, for the window (PRD §12)
// ---------------------------------------------------------------------------

/// A wall-clock stamp as `YYYY-MM-DD HH:MM`, in UTC.
///
/// UTC and not local time, deliberately: there is no time-zone database in the
/// dependency set, and a stamp silently rendered in the wrong zone is worse than
/// one honestly labelled `Z`. Session identity is what the operator recognises
/// a recording by (ADR-0014), so this is display only and nothing computes on it.
pub fn wall_time(at: WallTime) -> String {
    let ms = at.unix_millis();
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm) = (rem / 3_600_000, (rem % 3_600_000) / 60_000);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}Z")
}

/// A duration as `1h 04m`, `4m 12s`, or `820ms`.
pub fn duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    let secs = d.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3_600 {
        return format!("{}m {:02}s", secs / 60, secs % 60);
    }
    format!("{}h {:02}m", secs / 3_600, (secs % 3_600) / 60)
}

/// An event count, short enough to sit inline in the status strip.
///
/// Exact below a thousand, because the difference between `0` and `3` is the
/// whole diagnosis of a channel that is bound and silent. Abbreviated above it,
/// because by then the operator is reading "a lot" and the strip has four of
/// these to fit alongside everything else.
pub fn count(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // a display string, not a computation
    let f = n as f64;
    if n < 1_000 {
        format!("{n}")
    } else if n < 100_000 {
        format!("{:.1}k", f / 1_000.0)
    } else if n < 1_000_000 {
        format!("{}k", n / 1_000)
    } else {
        format!("{:.1}M", f / 1_000_000.0)
    }
}

/// A byte count as `1.2 MB`.
pub fn bytes(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // a display string, not a computation
    let f = n as f64;
    if n < 1_024 {
        format!("{n} B")
    } else if n < 1_024 * 1_024 {
        format!("{:.0} kB", f / 1_024.0)
    } else if n < 1_024 * 1_024 * 1_024 {
        format!("{:.1} MB", f / (1_024.0 * 1_024.0))
    } else {
        format!("{:.2} GB", f / (1_024.0 * 1_024.0 * 1_024.0))
    }
}

/// Howard Hinnant's `civil_from_days`, which is exact for every representable
/// day and needs no table.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use polis_events::{
        AgentType, EventKind, EventMeta, HookEvent, HookPayload, SessionId, ToolKind, ToolUseId,
        WorkerId,
    };

    use super::*;

    fn otel(body: OtelEvent) -> Event {
        let meta = EventMeta::now(Channel::Otel).with_session(SessionId::new("68160373-aaaa-bbbb"));
        Event::new(meta, Payload::Otel(Box::new(body)))
    }

    #[test]
    fn a_tool_result_prints_its_tool_outcome_and_path() {
        let event = otel(OtelEvent::ToolResult(Box::new(ToolCall {
            tool: ToolKind::Edit,
            tool_use_id: Some(ToolUseId::new("toolu_1")),
            paths: vec![(
                WorktreeId::PRIMARY,
                LogicalPath::new("src/normalize.rs").unwrap(),
            )],
            outcome: Outcome::Done,
            duration_ms: Some(12.0),
        })));
        let described = describe(&event);
        assert_eq!(described.kind, "tool_result");
        assert_eq!(described.detail, "Edit ok 12ms src/normalize.rs");
        assert_eq!(described.ident, "68160373/-/-");
        let rendered = line(1.5, &described);
        assert!(rendered.starts_with("    1.500  otel"), "{rendered}");
        assert!(rendered.ends_with("src/normalize.rs"), "{rendered}");
    }

    #[test]
    fn an_absent_worker_prints_as_absent_and_never_as_main_agent() {
        // With the beta traces channel off, EVERY tool call looks like this. A
        // renderer that printed "main" here would be the silent lie ADR-0006
        // exists to prevent.
        let described = describe(&otel(OtelEvent::AssistantResponse));
        assert!(described.ident.contains("/-/-"), "{}", described.ident);

        let mut meta = EventMeta::now(Channel::Otel).with_session(SessionId::new("s-1"));
        meta.worker = Some(WorkerId::new("a106e5fe86fce476a"));
        meta.agent_type = Some(AgentType::new("Explore"));
        let event = Event::new(meta, Payload::Otel(Box::new(OtelEvent::AssistantResponse)));
        assert_eq!(describe(&event).ident, "s-1/a106e5fe/-");
    }

    #[test]
    fn a_hook_prints_its_event_name_tool_and_recovered_paths() {
        let mut payload = HookPayload {
            hook_event_name: Some("PreToolUse".to_owned()),
            tool_name: Some("Write".to_owned()),
            ..HookPayload::default()
        };
        payload.extra.insert(
            polis_ingest::normalize::NORMALISED_PATHS_KEY.to_owned(),
            serde_json::json!([{ "worktree": 0, "path": "src/main.rs" }]),
        );
        let hook = HookEvent {
            kind: EventKind::PreToolUse,
            truncated: false,
            payload,
        };
        let event = Event::new(EventMeta::now(Channel::Hook), Payload::Hook(Box::new(hook)));
        let described = describe(&event);
        assert_eq!(described.kind, "hook.PreToolUse");
        assert_eq!(described.detail, "Write src/main.rs");
    }

    #[test]
    fn a_degraded_channel_prints_the_operator_reason_verbatim() {
        let event = Event::control(ControlEvent::ChannelDegraded {
            channel: Channel::Otel,
            reason: "127.0.0.1:4317 is already bound".to_owned(),
        });
        let described = describe(&event);
        assert_eq!(described.kind, "control.degraded");
        assert!(described.detail.contains("4317"), "{}", described.detail);
    }

    #[test]
    fn a_worktree_is_never_collapsed_into_the_primary() {
        let primary = (WorktreeId::PRIMARY, LogicalPath::new("a.rs").unwrap());
        let other = (WorktreeId(3), LogicalPath::new("a.rs").unwrap());
        assert_eq!(path_label(&primary), "a.rs");
        assert_eq!(
            path_label(&other),
            "wt3:a.rs",
            "two agents on two branches must not read as one"
        );
    }

    #[test]
    fn short_ids_never_panic_on_a_multi_byte_id() {
        assert_eq!(short(None), "-");
        assert_eq!(short(Some("")), "-");
        assert_eq!(short(Some("abc")), "abc");
        // Eight *characters*, not eight bytes: byte slicing here would panic.
        assert_eq!(short(Some("日本語のセッション識別子")).chars().count(), 8);
    }

    #[test]
    fn the_header_lines_up_with_what_line_prints() {
        let described = describe(&otel(OtelEvent::AssistantResponse));
        let rendered = line(0.0, &described);
        let head = header();
        // The channel column starts at the same offset in both, which is the
        // only alignment property a reader actually uses.
        assert_eq!(
            head.find("channel"),
            rendered.find("otel"),
            "\n{head}\n{rendered}"
        );
    }

    #[test]
    fn the_epoch_and_a_leap_day_render_correctly() {
        assert_eq!(
            wall_time(WallTime::from_unix_millis(0)),
            "1970-01-01 00:00Z"
        );
        // 2024-02-29T13:45:00Z
        assert_eq!(
            wall_time(WallTime::from_unix_seconds(1_709_214_300)),
            "2024-02-29 13:45Z"
        );
        // A stamp before the epoch must not wrap into the future.
        assert_eq!(
            wall_time(WallTime::from_unix_seconds(-1)),
            "1969-12-31 23:59Z"
        );
    }

    #[test]
    fn durations_read_at_every_scale() {
        assert_eq!(duration(Duration::from_millis(820)), "820ms");
        assert_eq!(duration(Duration::from_secs(42)), "42s");
        assert_eq!(duration(Duration::from_secs(252)), "4m 12s");
        assert_eq!(duration(Duration::from_mins(64)), "1h 04m");
        assert_eq!(duration(Duration::from_hours(2)), "2h 00m");
        assert_eq!(duration(Duration::from_secs(94_000)), "26h 06m");
    }

    #[test]
    fn byte_counts_read_at_every_scale() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(2_048), "2 kB");
        assert_eq!(bytes(3_145_728), "3.0 MB");
    }
}
