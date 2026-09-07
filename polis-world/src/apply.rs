//! Turning one [`polis_events::Event`] into world state (PRD §4.5, §5).
//!
//! One function per channel, dispatched from [`crate::World::apply`]. Nothing
//! here panics and nothing here is fatal: an unmodelled event, an unparsable
//! record and a missing field are all counted in [`crate::Health`] and dropped
//! (PRD §4.1, §4.4).
//!
//! # Where each fact comes from
//!
//! | Fact | Authoritative source | Fallback |
//! |---|---|---|
//! | thread identity | `session_id`, every channel | none needed; it is on everything |
//! | worker identity | `agent_id` on hooks, `agentId` in transcripts, `agent_id` on the tool span | the subagent transcript's own filename |
//! | worker parentage | `meta.json` `toolUseId`, or `wf_<runId>` | [`crate::WorkerAttribution::Unknown`], shown as uncertainty |
//! | pre-execution path | `PreToolUse` `tool_input.file_path` | the transcript's `tool_use.input` |
//! | diff lines | `structuredPatch` | the edit's own strings, marked approximate |
//! | line ranges | `structuredPatch` hunks | **none** — the tier degrades (ADR-0004) |
//! | verification | a shell command line ([`crate::verify`]) | none |
//!
//! The logs half of Channel A carries **no** `agent_id` at all — it is on the
//! `claude_code.tool` span and the `claude_code.llm_request` span only — so a
//! session with the beta traces channel off attributes every tool call to the
//! main agent. That is recorded in
//! [`crate::Health::subagent_attribution_degraded`], never guessed around
//! (ADR-0006).

use std::time::Instant;

use polis_events::{
    AgentType, Channel, ControlEvent, EventKind, EventMeta, FsEvent, HookEvent, LogicalPath,
    OtelEvent, OtelMetric, Outcome, ThreadId, ToolCall, ToolKind, ToolUseId, TranscriptEvent,
    TranscriptRecordKind, TranscriptSource, WorkerId, WorktreeId,
};
use polis_ingest::transcript::{Block, Content, SidecarRecord, ThreadedRecord};
use serde::Deserialize;
use serde_json::Value;

use crate::attention::{AttentionKind, DecisionSource};
use crate::contention::{Actor, Claim};

use crate::{
    verify, DiffPrecision, Observation, Operation, PathScope, ThreadStatus, UnattributedWorker,
    WorkerAttribution, World, AT_MENTION_WEIGHT,
};

/// How many in-flight tool calls are remembered before the oldest is dropped.
///
/// A `tool_use` whose `tool_result` never arrives — an interrupted session, a
/// transcript cut mid-turn — would otherwise leak. 4 096 is far above any real
/// turn's fan-out.
///
/// **It is no longer only a memory guard.** Since [`crate::IN_FLIGHT_MAX`], an
/// entry in this table is the evidence that keeps a thread out of `Idle` while a
/// long call runs, so the eviction at the cap now costs the owning thread its
/// in-flight standing and not merely a row: past 4 096 unsettled calls the
/// oldest thread starts reading `Idle` again mid-call, and its late
/// `tool_result` also loses the diff accounting [`settle`] does. Both failures
/// are silent. The number stays where it is — no real fan-out approaches it, and
/// raising it would trade a bound nothing hits for one nothing hits — but a
/// reader reaching for this constant should know it decides status now.
const PENDING_CAP: usize = 4_096;

/// A tool call seen starting, waiting for its result.
///
/// The join key is `tool_use_id`, which is the strongest cross-channel key Polis
/// has: it links `tool_decision`, `tool_result`, the `claude_code.tool` span,
/// hook payloads and the transcript's `tool_use` block.
#[derive(Debug, Clone)]
pub struct PendingCall {
    /// Who made it.
    pub thread: ThreadId,
    /// Which worker, if any.
    pub worker: Option<WorkerId>,
    /// Which tool.
    pub tool: ToolKind,
    /// The paths it named, already normalised.
    pub paths: Vec<(WorktreeId, LogicalPath)>,
    /// Whether the command line is a test run ([`crate::verify`]).
    pub verification: bool,
    /// Line delta approximated from the call's own strings, for the 65% of
    /// subagent results that will arrive with no `structuredPatch`.
    pub approx_diff: Option<(u32, u32)>,
    /// When it started.
    pub started: Instant,
}

// ---------------------------------------------------------------------------
// Channel A — OpenTelemetry
// ---------------------------------------------------------------------------

/// Applies one OTel log record or span.
pub(crate) fn otel(world: &mut World, meta: &EventMeta, event: &OtelEvent) {
    let at = meta.observed;
    let Some(session) = meta.session.clone() else {
        world.health.events_ignored += 1;
        return;
    };
    let thread_id = ThreadId::of_session(session.clone());
    world.thread_entry(&session, at);
    if let Some(kind) = meta.agent_type.clone() {
        if meta.worker.is_none() {
            // `claude --agent foo`: an agent *type* on a main agent, which is
            // never a worker discriminator (ADR-0030).
            if let Some(t) = world.threads.get_mut(&thread_id) {
                t.agent_type = Some(kind);
            }
        }
    }
    if let Some(worker) = meta.worker.clone() {
        attach_worker(
            world,
            &thread_id,
            &worker,
            meta.agent_type.clone(),
            at,
            WorkerAttribution::RecordAgentId,
        );
    }

    match event {
        OtelEvent::ToolResult(call) => {
            if meta.worker.is_none() {
                world.health.otel_tool_results_without_worker += 1;
                if world.health.otel_tool_spans == 0 {
                    // Every tool call is being attributed to the main agent
                    // because the only channel that could say otherwise is off.
                    world.health.subagent_attribution_degraded = true;
                }
            }
            tool_call(world, &thread_id, meta.worker.as_ref(), call, at);
        }
        OtelEvent::ToolSpan { call, .. } => {
            world.health.otel_tool_spans += 1;
            world.health.subagent_attribution_degraded = false;
            tool_call(world, &thread_id, meta.worker.as_ref(), call, at);
        }
        OtelEvent::UserPrompt { .. } | OtelEvent::InteractionSpan { .. } => {
            working(world, &thread_id, at);
        }
        OtelEvent::ToolDecision { decision, .. } => {
            // The human answered, one way or the other.
            world.resolve_decisions(&thread_id);
            working(world, &thread_id, at);
            if decision.as_deref() == Some("reject") {
                if let Some(t) = world.threads.get_mut(&thread_id) {
                    t.failures = t.failures.saturating_add(1);
                }
            }
        }
        OtelEvent::ToolBlockedOnUserSpan { .. } => {
            // Measured *after* the wait ended, so it cannot raise a pin — but it
            // is proof the thread is unblocked now.
            world.resolve_decisions(&thread_id);
        }
        OtelEvent::SubagentCompleted { agent_type, .. } => {
            if let Some(worker) = meta.worker.clone() {
                finish_worker(world, &thread_id, &worker, agent_type.clone(), at);
            }
        }
        OtelEvent::PermissionModeChanged { mode } => {
            if let Some(t) = world.threads.get_mut(&thread_id) {
                t.permission_mode.clone_from(mode);
            }
        }
        OtelEvent::AtMention => {
            // ADR-0017: the event proves an `@`-mention happened and carries no
            // path, so it cannot become an observation. Counted so the blind
            // spot is visible rather than silent.
            world.health.at_mentions += 1;
            world.health.at_mentions_without_path += 1;
        }
        OtelEvent::ApiError { .. } | OtelEvent::ApiRefusal | OtelEvent::ApiRetriesExhausted => {
            if let Some(t) = world.threads.get_mut(&thread_id) {
                t.failures = t.failures.saturating_add(1);
            }
        }
        OtelEvent::Unknown { name } => {
            tracing::debug!(event = %name, "unmodelled OTel event");
            world.health.drift += 1;
        }
        _ => world.health.events_ignored += 1,
    }
}

/// Applies one OTLP metric data point.
///
/// Deliberately dropped, with the reason written down: PRD §2 makes cost charts
/// and token burndowns a **non-goal** ("there are good OTel stacks for that
/// already"), and ADR-0040 already routes the one number Polis does show — live
/// tokens — to the `api_request` event rather than to this stream, which lags by
/// a whole export interval. Accumulating eight DELTA counters nothing renders
/// would be a maintenance burden with no pixel behind it.
pub(crate) fn metric(world: &mut World, metric: &OtelMetric) {
    tracing::trace!(value = metric.value, "metric dropped: PRD §2 non-goal");
    world.health.events_ignored += 1;
}

/// Applies one tool call, whichever channel assembled it.
fn tool_call(
    world: &mut World,
    thread: &ThreadId,
    worker: Option<&WorkerId>,
    call: &ToolCall,
    at: Instant,
) {
    working(world, thread, at);
    // The span carries `file_path` only — never a cwd — so a shell call arrives
    // here with no paths at all and lands on rung 3, which is the honest answer
    // for this channel.
    let first = call.paths.first().map(|(_, p)| p.clone());
    let placement = world.placement_for(first.as_ref(), None);
    let op = Operation {
        path: first,
        placement,
        tool: call.tool.clone(),
        glyph: call.tool.glyph(),
        outcome: call.outcome,
        worker: worker.cloned(),
        at,
        tool_use: call.tool_use_id.clone(),
    };
    if let Some(t) = world.threads.get_mut(thread) {
        t.tool_calls = t.tool_calls.saturating_add(1);
        if call.outcome == Outcome::Failed {
            t.failures = t.failures.saturating_add(1);
        }
        t.push_op(op.clone());
    }
    for (worktree, path) in &call.paths {
        record_path(world, thread, worker, &call.tool, *worktree, path, at);
        if call.tool.is_mutating() && call.outcome.is_settled() {
            // Channel A is one of the two landing triggers the 30 s TTL backs up
            // (ADR-0044). Its logs never carry `agent_id`, so on a subagent's
            // edit this names the main agent and matches nothing — which is
            // correct, and the TTL is the backstop.
            //
            // A failed call wrote nothing, so its reservation is released; a
            // successful one *did* write, and its claim stays for the rest of
            // the TTL because that is when a sibling can write over it.
            let actor = actor_of(thread, worker);
            if call.outcome == Outcome::Failed {
                world.claims.release(&actor, path);
            } else {
                world.claims.landed(&actor, path, at);
            }
        }
    }
    // After the observations, not before: rung 3 asks where the *thread* is, and
    // this call's own step is part of that answer.
    world.census_op(thread, &op);
}

// ---------------------------------------------------------------------------
// Channel B — hooks
// ---------------------------------------------------------------------------

/// Applies one hook datagram.
#[allow(clippy::too_many_lines)] // one arm per registered event; splitting hides the set
pub(crate) fn hook(world: &mut World, meta: &EventMeta, hook: &HookEvent) {
    let at = meta.observed;
    let payload = &hook.payload;
    let Some(session) = payload.session_id.clone().or_else(|| meta.session.clone()) else {
        world.health.events_ignored += 1;
        return;
    };
    let thread_id = ThreadId::of_session(session.clone());
    world.thread_entry(&session, at);
    if let Some(cwd) = payload.cwd.as_deref() {
        note_cwd(world, &thread_id, cwd);
    }

    // Presence of `agent_id`, and nothing else, is the worker discriminator
    // (ADR-0030). `agent_type` is present for `--agent` main sessions too.
    let worker = payload.agent_id.clone();
    if let Some(w) = &worker {
        attach_worker(
            world,
            &thread_id,
            w,
            payload.agent_type.clone(),
            at,
            WorkerAttribution::RecordAgentId,
        );
    } else if let Some(kind) = payload.agent_type.clone() {
        if let Some(t) = world.threads.get_mut(&thread_id) {
            t.agent_type = Some(kind);
        }
    }

    // `hook_event_name` in the payload is authoritative over the wire tag.
    let kind = if payload.hook_event_name.is_some() {
        payload.kind()
    } else {
        hook.kind
    };

    match kind {
        EventKind::SessionStart => working(world, &thread_id, at),
        EventKind::SessionEnd => finish_thread(world, &thread_id, at, true),
        EventKind::PreToolUse => {
            // The only authoritative *pre-execution* path source Polis has:
            // `tool_decision` on Channel A carries no path (ADR-0004).
            let tool = payload
                .tool_name
                .as_deref()
                .map_or(ToolKind::Other(String::new()), ToolKind::parse);
            let cwd = payload.cwd.clone();
            let inputs = payload.tool_input.as_ref();
            for (raw, scope, _) in tool_input_paths(&tool, inputs, cwd.as_deref()) {
                let Some((worktree, path)) = world.resolve_path(cwd.as_deref(), &raw) else {
                    continue;
                };
                observe(world, &thread_id, worker.as_ref(), &tool, scope, &path, at);
                record_file(world, &thread_id, &tool, &path, at);
                if tool.is_mutating() {
                    let branch = payload
                        .extra
                        .get("gitBranch")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let claim = Claim::write(thread_id.clone(), path.clone(), at)
                        .in_checkout(worktree, branch.as_deref())
                        .by_worker(worker.clone());
                    register_claim(world, claim);
                }
            }
            if let Some(command) = payload
                .tool_input
                .as_ref()
                .and_then(|v| v.get("command"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
            {
                shell_evidence(
                    world,
                    &thread_id,
                    worker.as_ref(),
                    &tool,
                    cwd.as_deref(),
                    &command,
                    at,
                );
            }
            working(world, &thread_id, at);
        }
        EventKind::PostToolUseFailure => {
            if let Some(t) = world.threads.get_mut(&thread_id) {
                t.failures = t.failures.saturating_add(1);
            }
            // A failure is a *completion*: the tool ran and came back. It is
            // exactly as much proof that the thread is alive as the success
            // above, which stamps `working` on its last line — and it is the
            // authoritative edge Channel B has for the end of a call that took
            // minutes to fail. Without this a hook deployment drew a thread
            // `Idle` for the whole of a failing build and then, on the next
            // record, working again (`crate::IDLE_AFTER`).
            working(world, &thread_id, at);
        }
        EventKind::PermissionRequest => {
            needs_decision(
                world,
                &thread_id,
                DecisionSource::PermissionRequest,
                at,
                None,
            );
        }
        EventKind::Elicitation => {
            needs_decision(world, &thread_id, DecisionSource::Elicitation, at, None);
        }
        EventKind::ElicitationResult => {
            world.resolve_decisions(&thread_id);
            working(world, &thread_id, at);
        }
        EventKind::TeammateIdle => {
            needs_decision(world, &thread_id, DecisionSource::TeammateIdle, at, None);
        }
        EventKind::Notification => {
            needs_decision(world, &thread_id, DecisionSource::Notification, at, None);
        }
        EventKind::SubagentStart => {
            if let Some(w) = &worker {
                attach_worker(
                    world,
                    &thread_id,
                    w,
                    payload.agent_type.clone(),
                    at,
                    WorkerAttribution::RecordAgentId,
                );
            }
        }
        EventKind::SubagentStop => {
            // `SubagentStop` is noise on the attention layer — a main thread
            // going idle is news (PRD §11.2) — but it is the lowest-latency
            // lifecycle signal there is, and it arrives with `agent_id` already
            // attached (ADR-0013).
            if let Some(w) = &worker {
                finish_worker(world, &thread_id, w, payload.agent_type.clone(), at);
            }
        }
        EventKind::Stop => {
            if payload.is_worker() {
                // A worker stopping is not a thread finishing.
                if let Some(w) = &worker {
                    finish_worker(world, &thread_id, w, payload.agent_type.clone(), at);
                }
            } else {
                finish_thread(world, &thread_id, at, true);
            }
        }
        EventKind::StopFailure => {
            // The turn ended on an API error: quiet, but not finished, and
            // certainly not "done".
            if let Some(t) = world.threads.get_mut(&thread_id) {
                t.status = ThreadStatus::Idle;
                t.failures = t.failures.saturating_add(1);
            }
        }
        EventKind::CwdChanged => {
            if let Some(new) = payload
                .extra
                .get("new_cwd")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
            {
                note_cwd(world, &thread_id, &new);
            }
        }
        // Registered, understood, and deliberately not modelled: none of these
        // changes anything the map draws.
        EventKind::WorktreeRemove
        | EventKind::TaskCreated
        | EventKind::TaskCompleted
        | EventKind::PreCompact
        | EventKind::PostCompact => world.health.events_ignored += 1,
        EventKind::Unknown => {
            tracing::debug!(name = ?payload.hook_event_name, "unmodelled hook event");
            world.health.drift += 1;
        }
        // `EventKind` is `#[non_exhaustive]`: a twentieth registration is drift.
        _ => world.health.drift += 1,
    }

    if hook.truncated {
        // The sender cut the payload at 60 KiB (ADR-0023): treat as
        // notification-only and let Channel D backfill.
        world.health.drift += 1;
    }
}

// ---------------------------------------------------------------------------
// Channel C — filesystem
// ---------------------------------------------------------------------------

/// Applies one filesystem mutation.
///
/// > The filesystem does not know which agent wrote. (PRD §4.3)
///
/// So this never attributes and never raises attention. It updates disk truth —
/// which is what makes a file edited outside Claude Code still show up — and
/// marks a claim landed when the write it was covering reaches the disk.
pub(crate) fn fs(world: &mut World, meta: &EventMeta, event: &FsEvent) {
    let at = meta.observed;
    match event {
        FsEvent::Created { path } | FsEvent::Modified { path } => {
            let file = world.file_entry(&path.1);
            file.last_touched = Some(at);
            file.deleted = false;
            land_sole_claim(world, &path.1, at);
        }
        FsEvent::Removed { path } => {
            // A vacant lot that goes to seed rather than vanishing (PRD §7.5).
            let file = world.file_entry(&path.1);
            file.deleted = true;
            file.last_touched = Some(at);
        }
        FsEvent::Renamed { from, to } => {
            let carried = world.files.remove(&from.1);
            let file = world.file_entry(&to.1);
            if let Some(prev) = carried {
                *file = prev;
            }
            file.last_touched = Some(at);
            file.deleted = false;
        }
        FsEvent::RescanRequired => {
            world.health.degraded.insert(
                Channel::Fs.to_string(),
                "the watcher lost events; the tree needs a re-scan".to_owned(),
            );
        }
        // `FsEvent` is `#[non_exhaustive]`: `notify` 9 will add vocabulary.
        _ => world.health.drift += 1,
    }
}

/// Marks a claim landed when exactly one actor holds the path.
///
/// > The filesystem does not know which agent wrote. (PRD §4.3)
///
/// With two or more live claims the write is not evidence about *which* of them
/// landed, and restarting the wrong one's TTL would be a small lie about the
/// only state the operator has to be able to trust. The 30 s TTL handles that
/// case, and Channel D reports the landing per actor anyway.
fn land_sole_claim(world: &mut World, path: &LogicalPath, at: Instant) {
    let sole = match world.claims.claims_on(path) {
        [only] => Some(only.actor()),
        _ => None,
    };
    if let Some(actor) = sole {
        world.claims.landed(&actor, path, at);
    }
}

/// The claim key for a thread and the worker that acted inside it.
fn actor_of(thread: &ThreadId, worker: Option<&WorkerId>) -> Actor {
    Actor {
        thread: thread.clone(),
        worker: worker.cloned(),
    }
}

// ---------------------------------------------------------------------------
// Channel D — JSONL transcripts
// ---------------------------------------------------------------------------

/// Applies one transcript record.
pub(crate) fn transcript(world: &mut World, meta: &EventMeta, event: &TranscriptEvent) {
    let at = meta.observed;
    match event.kind {
        TranscriptRecordKind::Assistant | TranscriptRecordKind::User => {
            let Some(session) = meta.session.clone() else {
                world.health.events_ignored += 1;
                return;
            };
            let Ok(record) = ThreadedRecord::deserialize(&event.record) else {
                world.health.drift += 1;
                return;
            };
            let thread_id = ThreadId::of_session(session.clone());
            world.thread_entry(&session, at);
            envelope(world, &thread_id, &record);
            let worker = worker_of(world, &thread_id, meta, &event.source, &record, at);
            if event.kind == TranscriptRecordKind::Assistant {
                assistant(world, &thread_id, worker.as_ref(), &record, at);
                turn_boundary(world, &thread_id, worker.as_ref(), &record, at);
            } else {
                user(world, &thread_id, worker.as_ref(), &record, at);
                answered(
                    world,
                    &thread_id,
                    worker.as_ref(),
                    &event.record,
                    &record,
                    at,
                );
            }
        }
        TranscriptRecordKind::Attachment => {
            let Some(session) = meta.session.clone() else {
                world.health.events_ignored += 1;
                return;
            };
            let Ok(record) = ThreadedRecord::deserialize(&event.record) else {
                world.health.drift += 1;
                return;
            };
            let thread_id = ThreadId::of_session(session.clone());
            world.thread_entry(&session, at);
            envelope(world, &thread_id, &record);
            let worker = worker_of(world, &thread_id, meta, &event.source, &record, at);
            attachment(world, &thread_id, worker.as_ref(), &record, at);
        }
        TranscriptRecordKind::Started | TranscriptRecordKind::Result => {
            journal(world, meta, event, at);
        }
        TranscriptRecordKind::System => world.health.events_ignored += 1,
        TranscriptRecordKind::Unknown => {
            tracing::debug!(offset = event.byte_offset, "unmodelled transcript record");
            world.health.drift += 1;
        }
        _ => {
            let Some(session) = meta.session.clone() else {
                world.health.events_ignored += 1;
                return;
            };
            let Ok(record) = SidecarRecord::deserialize(&event.record) else {
                world.health.drift += 1;
                return;
            };
            let thread_id = ThreadId::of_session(session.clone());
            world.thread_entry(&session, at);
            sidecar(world, &thread_id, &event.kind, &record, at);
        }
    }
}

/// Applies the envelope every threaded record carries.
fn envelope(world: &mut World, thread: &ThreadId, record: &ThreadedRecord) {
    if let Some(cwd) = record.envelope.cwd.as_deref() {
        note_cwd(world, thread, cwd);
    }
    let branch = record
        .envelope
        .git_branch
        .as_deref()
        .filter(|b| !b.is_empty() && *b != "HEAD")
        .map(ToOwned::to_owned);
    if let Some(t) = world.threads.get_mut(thread) {
        if branch.is_some() {
            t.branch = branch;
        }
        if let Some(slug) = record.envelope.slug.as_deref() {
            if t.title.is_none() {
                t.title = Some(slug.to_owned());
            }
        }
    }
}

/// Resolves the worker a transcript record came from, attaching it to its thread
/// or parking it as unattributed.
fn worker_of(
    world: &mut World,
    thread: &ThreadId,
    meta: &EventMeta,
    source: &TranscriptSource,
    record: &ThreadedRecord,
    at: Instant,
) -> Option<WorkerId> {
    let (id, evidence) = match (&meta.worker, source) {
        (Some(w), _) if record.envelope.agent_id.is_some() => {
            (w.clone(), WorkerAttribution::RecordAgentId)
        }
        (Some(w), TranscriptSource::Subagent { .. }) => {
            (w.clone(), WorkerAttribution::TranscriptFile)
        }
        (Some(w), _) => (w.clone(), WorkerAttribution::RecordAgentId),
        (None, _) => return None,
    };
    // `attributionAgent` is the agent *type*, never an id (ADR-0030).
    let kind = record
        .attribution_agent
        .as_deref()
        .map(AgentType::new)
        .or_else(|| meta.agent_type.clone());
    let evidence = match source {
        TranscriptSource::Subagent {
            workflow_run: Some(run),
            ..
        } => {
            // Route 3: the only parent link 503 of 643 subagents have.
            if world.workflow_runs.contains_key(run) {
                WorkerAttribution::WorkflowRun
            } else {
                evidence
            }
        }
        _ => evidence,
    };
    attach_worker(world, thread, &id, kind, at, evidence);
    if let Some(w) = world
        .threads
        .get_mut(thread)
        .and_then(|t| t.worker_mut(&id))
    {
        if let TranscriptSource::Subagent {
            workflow_run: Some(run),
            ..
        } = source
        {
            w.workflow_run = Some(run.clone());
        }
    }
    Some(id)
}

/// Applies an `assistant` record: `tool_use` blocks are where the work is.
fn assistant(
    world: &mut World,
    thread: &ThreadId,
    worker: Option<&WorkerId>,
    record: &ThreadedRecord,
    at: Instant,
) {
    working(world, thread, at);
    for block in blocks(record) {
        if block.kind.as_deref() != Some("tool_use") {
            continue;
        }
        let Some(name) = block.name.as_deref() else {
            world.health.drift += 1;
            continue;
        };
        let tool = ToolKind::parse(name);
        let input = block.input.as_ref();
        let command = input
            .and_then(|v| v.get("command"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        // The agent's own sentence about this call, and the only prose that
        // gets past this line. `command` above is read for its *shape* — is
        // this a test run — and is never stored; a `description` is stored, so
        // it is vetted first. See `crate::Intent`.
        if let Some(intent) = intent_of(&tool, input, at) {
            if let Some(t) = world.threads.get_mut(thread) {
                t.push_intent(intent);
            }
        }
        let verification = command
            .as_deref()
            .is_some_and(verify::is_verification_command);
        let glyph = command
            .as_deref()
            .map_or_else(|| tool.glyph(), |c| verify::glyph_for(&tool, Some(c)));
        let cwd = record.envelope.cwd.clone();
        let branch = record.envelope.git_branch.clone();

        let mut resolved: Vec<(WorktreeId, LogicalPath)> = Vec::new();
        // Kept apart because they answer different questions: the tool's own
        // paths are rung 1 of `place`'s chain, the working directory is rung 2,
        // and conflating them is what put every shell call a session ever ran on
        // one pixel at the centre of the map.
        let mut own: Option<LogicalPath> = None;
        let mut from_cwd: Option<LogicalPath> = None;
        for (raw, scope, origin) in tool_input_paths(&tool, input, cwd.as_deref()) {
            let Some((worktree, path)) = world.resolve_path(cwd.as_deref(), &raw) else {
                continue;
            };
            match origin {
                PathOrigin::Input => {
                    if own.is_none() {
                        own = Some(path.clone());
                    }
                }
                PathOrigin::Cwd => from_cwd = Some(path.clone()),
            }
            observe(world, thread, worker, &tool, scope, &path, at);
            record_file(world, thread, &tool, &path, at);
            if tool.is_mutating() {
                let claim = Claim::write(thread.clone(), path.clone(), at)
                    .in_checkout(worktree, branch.as_deref())
                    .by_worker(worker.cloned());
                register_claim(world, claim);
            } else if tool == ToolKind::Read {
                let probe = Claim::read(thread.clone(), path.clone(), at)
                    .in_checkout(worktree, branch.as_deref());
                if world.claims.note_read(&probe).is_some() {
                    world.health.contention_without_line_ranges += 1;
                }
            }
            resolved.push((worktree, path));
        }

        // PRD §6.1's shell row, read sharply: the paths the command itself
        // names. Scope evidence only — the operation still places by `cwd` or on
        // its thread, because "what is this thread working on" and "where does
        // this mark go" are different questions (`crate::shell`).
        if let Some(command) = command.as_deref() {
            shell_evidence(world, thread, worker, &tool, cwd.as_deref(), command, at);
        }

        let op = Operation {
            path: own.clone().or_else(|| from_cwd.clone()),
            placement: world.placement_for(own.as_ref(), from_cwd.as_ref()),
            tool: tool.clone(),
            glyph,
            outcome: Outcome::Pending,
            worker: worker.cloned(),
            at,
            tool_use: block.id.as_deref().map(ToolUseId::new),
        };
        if let Some(t) = world.threads.get_mut(thread) {
            t.tool_calls = t.tool_calls.saturating_add(1);
            t.push_op(op.clone());
        }
        world.census_op(thread, &op);

        if let Some(id) = block.id.as_deref().map(ToolUseId::new) {
            if tool == ToolKind::Agent || tool == ToolKind::Workflow {
                world.spawns.insert(id.clone(), thread.clone());
            }
            let approx_diff = approximate_diff(&tool, input);
            insert_pending(
                world,
                id,
                PendingCall {
                    thread: thread.clone(),
                    worker: worker.cloned(),
                    tool,
                    paths: resolved,
                    verification,
                    approx_diff,
                    started: at,
                },
            );
        }
    }
}

/// Applies a `user` record: `tool_result` blocks settle calls, and
/// `toolUseResult` carries everything the sidecar has when it is there at all.
fn user(
    world: &mut World,
    thread: &ThreadId,
    worker: Option<&WorkerId>,
    record: &ThreadedRecord,
    at: Instant,
) {
    let sidecar = record.tool_use_result.as_ref();
    for block in blocks(record) {
        if block.kind.as_deref() != Some("tool_result") {
            continue;
        }
        // `is_error` is on 49.9% of blocks and **absent means not an error**;
        // there is no third state.
        let outcome = Outcome::from_is_error(block.is_error);
        let Some(id) = block.tool_use_id.as_deref().map(ToolUseId::new) else {
            world.health.drift += 1;
            continue;
        };
        settle(world, thread, worker, &id, outcome, sidecar, at);
    }
}

/// Settles one tool call against its result.
fn settle(
    world: &mut World,
    thread: &ThreadId,
    worker: Option<&WorkerId>,
    id: &ToolUseId,
    outcome: Outcome,
    sidecar: Option<&Value>,
    at: Instant,
) {
    let pending = world.pending.remove(id);
    // Where the failure lands, so PRD §10.2's red is countable even when it is
    // not visible. A result whose operation has already fallen out of `OPS_CAP`
    // has nowhere to put its outcome and is counted rather than shrugged at.
    let mut settled: Option<Operation> = None;
    let mut evicted = false;
    if let Some(t) = world.threads.get_mut(thread) {
        if let Some(op) = t
            .ops
            .iter_mut()
            .rev()
            .find(|op| op.tool_use.as_ref() == Some(id))
        {
            op.outcome = outcome;
            settled = Some(op.clone());
        } else {
            evicted = true;
        }
        if outcome == Outcome::Failed {
            t.failures = t.failures.saturating_add(1);
        }
    }
    if outcome == Outcome::Failed {
        match &settled {
            Some(op) => world.census_op_failed(thread, op),
            None => world.health.ops_failed.count(4),
        }
    }
    if evicted {
        world.health.ops_settled_after_eviction =
            world.health.ops_settled_after_eviction.saturating_add(1);
    }

    // A subagent spawn, whichever shape the result took (ADR-0013 routes 1-3).
    if let Some(sidecar) = sidecar {
        spawn_from_result(world, thread, id, sidecar, at);
        // A job that outlives the turn that launched it. Opened here because
        // this result is the only record that names its id.
        open_background(world, thread, id.as_str(), sidecar, at);
    }

    let Some(pending) = pending else {
        return;
    };

    let backgrounded = sidecar.is_some_and(|s| s.get("backgroundTaskId").is_some());
    if pending.verification && outcome != Outcome::Failed && !backgrounded {
        // A backgrounded test run has *started*, not passed. Marking the thread
        // verified here would let `cargo test --workspace &` clear PRD §11.2's
        // "done, unverified" the instant it was launched — the opposite of what
        // the operator meant by "waiting on the clean run before committing".
        // The real verdict arrives with the `<task-notification>`.
        mark_verified(world, thread, at);
    }

    // Diff accounting, exact where `structuredPatch` survived and approximate
    // where it did not — which is 65% of subagent results (ADR-0004).
    let patch = sidecar.and_then(|s| s.get("structuredPatch"));
    let exact = patch.and_then(patch_totals);
    // The branch the *claim* has to carry, or the line-range upgrade below tiers
    // its own thread's edit as `Medium` ("different branches") when it is on the
    // one branch this session has ever been on. `PendingCall` does not carry a
    // branch; the thread does, and it is the same one the original claim used.
    let branch = world.threads.get(thread).and_then(|t| t.branch.clone());
    for (worktree, path) in &pending.paths {
        if pending.tool.is_mutating() {
            if let Some((added, removed, range)) = exact {
                let file = world.file_entry(path);
                file.add_diff(added, removed, DiffPrecision::Exact);
                // Upgrade the claim now that a line range exists — the Critical
                // tier only becomes reachable here — and register it as
                // **already landed**, because `structuredPatch` is proof the
                // write happened. Registering it as pending and then landing it
                // would be two statements about one fact.
                let claim = Claim::write(thread.clone(), path.clone(), at)
                    .in_checkout(*worktree, branch.as_deref())
                    .with_lines(range.0, range.1)
                    .by_worker(worker.cloned())
                    .already_landed();
                register_claim(world, claim);
            } else if let Some((added, removed)) = pending.approx_diff {
                // An edit with no `structuredPatch`, which is 65 % of subagent
                // results. It is *not* a contention hit — nothing collided here
                // — so it belongs to its own counter (ADR-0004).
                world.health.edits_without_line_ranges += 1;
                let file = world.file_entry(path);
                file.add_diff(added, removed, DiffPrecision::Approximate);
            }
        }
        if let Some(total) = sidecar
            .and_then(|s| s.get("file"))
            .and_then(|f| f.get("totalLines"))
            .and_then(Value::as_u64)
        {
            world.file_entry(path).total_lines = u32::try_from(total).ok();
        }
        if pending.tool.is_mutating() && outcome.is_settled() {
            // A failed edit wrote nothing, so its reservation goes; a successful
            // one is a hazard for the rest of the TTL. This one line is what
            // took the operator's real collisions from zero to visible: a
            // transcript's `tool_result` lands under a second after its
            // `tool_use`, and deleting the claim there meant two workers 4.7 s
            // apart never held claims at the same instant.
            let actor = actor_of(thread, worker);
            if outcome == Outcome::Failed {
                world.claims.release(&actor, path);
            } else {
                world.claims.landed(&actor, path, at);
            }
        }
    }

    // `toolStats` on a sync `Agent` result is a ready-made per-subagent roll-up.
    if let Some(stats) = sidecar.and_then(|s| s.get("toolStats")) {
        let added = stats.get("linesAdded").and_then(Value::as_u64).unwrap_or(0);
        let removed = stats
            .get("linesRemoved")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if let Some(t) = world.threads.get_mut(thread) {
            t.lines_added = t
                .lines_added
                .saturating_add(u32::try_from(added).unwrap_or(u32::MAX));
            t.lines_removed = t
                .lines_removed
                .saturating_add(u32::try_from(removed).unwrap_or(u32::MAX));
        }
    }
}

/// Reads a spawn out of a tool result: `Agent` gives `agentId`, `Workflow` gives
/// `runId`.
fn spawn_from_result(
    world: &mut World,
    thread: &ThreadId,
    id: &ToolUseId,
    sidecar: &Value,
    at: Instant,
) {
    if let Some(agent) = sidecar.get("agentId").and_then(Value::as_str) {
        let worker = WorkerId::new(agent);
        let kind = sidecar
            .get("agentType")
            .and_then(Value::as_str)
            .map(AgentType::new);
        attach_worker(
            world,
            thread,
            &worker,
            kind,
            at,
            WorkerAttribution::SpawnToolUse,
        );
        // ADR-0019: `async_launched` is a launch acknowledgement, never a
        // terminal state. Only `completed` finishes a worker.
        let status = sidecar.get("status").and_then(Value::as_str);
        if let Some(w) = world
            .threads
            .get_mut(thread)
            .and_then(|t| t.worker_mut(&worker))
        {
            w.spawn_tool_use = Some(id.clone());
            w.running = status != Some("completed");
            w.last_activity = at;
        }
    }
    if let Some(run) = sidecar.get("runId").and_then(Value::as_str) {
        let run = workflow_run_key(run);
        world.workflow_runs.insert(run.to_owned(), thread.clone());
        adopt_workflow_run(world, run, thread, at);
    }
}

/// Applies an `attachment` record — the only transcript evidence of a file the
/// operator pinned with `@` (ADR-0017).
fn attachment(
    world: &mut World,
    thread: &ThreadId,
    worker: Option<&WorkerId>,
    record: &ThreadedRecord,
    at: Instant,
) {
    let Some(attachment) = &record.attachment else {
        world.health.events_ignored += 1;
        return;
    };
    let kind = attachment.kind.as_deref().unwrap_or_default();
    if !matches!(
        kind,
        "file" | "nested_memory" | "edited_text_file" | "compact_file_reference"
    ) {
        world.health.events_ignored += 1;
        return;
    }
    world.health.at_mentions += 1;
    let raw = attachment
        .fields
        .get("filename")
        .or_else(|| attachment.fields.get("path"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let Some(raw) = raw else {
        world.health.at_mentions_without_path += 1;
        return;
    };
    let cwd = record.envelope.cwd.clone();
    let Some((_, path)) = world.resolve_path(cwd.as_deref(), &raw) else {
        world.health.at_mentions_without_path += 1;
        return;
    };
    let obs = Observation {
        thread: thread.clone(),
        worker: worker.cloned(),
        path: path.clone(),
        tool: ToolKind::Read,
        scope: PathScope::File,
        at,
        weight: Some(AT_MENTION_WEIGHT),
    };
    world.observe(&obs);
    let file = world.file_entry(&path);
    file.reads = file.reads.saturating_add(1);
}

/// Applies one of the fifteen flat session sidecars.
fn sidecar(
    world: &mut World,
    thread: &ThreadId,
    kind: &TranscriptRecordKind,
    record: &SidecarRecord,
    at: Instant,
) {
    match kind {
        TranscriptRecordKind::CostState => {
            // Session-cumulative and written at session end: a reconciliation
            // input, not a live one (ADR-0042).
            let added = record.total_lines_added.unwrap_or(0).max(0);
            let removed = record.total_lines_removed.unwrap_or(0).max(0);
            if let Some(t) = world.threads.get_mut(thread) {
                t.lines_added = t.lines_added.max(u32::try_from(added).unwrap_or(u32::MAX));
                t.lines_removed = t
                    .lines_removed
                    .max(u32::try_from(removed).unwrap_or(u32::MAX));
            }
        }
        TranscriptRecordKind::PermissionMode | TranscriptRecordKind::Mode => {
            let mode = record
                .permission_mode
                .clone()
                .or_else(|| record.mode.clone());
            if let Some(t) = world.threads.get_mut(thread) {
                if mode.is_some() {
                    t.permission_mode = mode;
                }
            }
        }
        TranscriptRecordKind::AiTitle
        | TranscriptRecordKind::CustomTitle
        | TranscriptRecordKind::AgentName => {
            let title = record
                .custom_title
                .clone()
                .or_else(|| record.ai_title.clone())
                .or_else(|| record.agent_name.clone());
            if let Some(t) = world.threads.get_mut(thread) {
                if title.is_some() {
                    t.title = title;
                }
            }
        }
        TranscriptRecordKind::FileHistoryDelta => {
            // Claude Code's own undo bookkeeping: a second, independent list of
            // files an agent touched. Disk truth, not evidence of intent, so it
            // touches the file and feeds no territory.
            if let Some(raw) = record.tracking_path.clone() {
                if let Some((_, path)) = world.resolve_path(None, &raw) {
                    world.file_entry(&path).touch(thread, at);
                }
            }
        }
        _ => world.health.events_ignored += 1,
    }
}

/// Applies a `journal.jsonl` line — the cheapest liveness signal a workflow
/// fleet has.
fn journal(world: &mut World, meta: &EventMeta, event: &TranscriptEvent, at: Instant) {
    let TranscriptSource::WorkflowJournal { run } = &event.source else {
        world.health.events_ignored += 1;
        return;
    };
    let Some(agent) = event.record.get("agentId").and_then(Value::as_str) else {
        world.health.drift += 1;
        return;
    };
    let worker = WorkerId::new(agent);
    // Three rungs, strongest first. The run table is the parent `Workflow`
    // call's own word. The worker's own transcript is the subagent's, and it
    // usually wins the race: the journal is read last, because it has no
    // timestamps to sort by, so by the time it arrives the subagent's file has
    // normally already placed it.
    //
    // Rung 3 is the journal file's own directory. `polis_ingest` fills
    // `meta.session` from the path when the record does not name one, and a
    // journal record never does — so for a workflow agent this is the session
    // whose `subagents/workflows/wf_<run>/` the line was read out of.
    //
    // It is last because it is the weakest of the three: the run table is the
    // parent `Workflow` call's own word, and the worker's transcript is the
    // subagent's. But it is the only rung that survives the common failure —
    // live tailing opens an existing file at its end, so a Polis started after
    // the `Workflow` call never reads the one record that carries the run id,
    // and before this every agent of that run parked for ever on evidence that
    // was still sitting in the directory name. Attribution is
    // `WorkerAttribution::TranscriptFile`, which is exactly what it is: the
    // file's path rather than a record field, reliable in practice and weaker
    // in principle, and `strength` keeps it from overwriting a real match.
    let placed = world
        .workflow_runs
        .get(run)
        .cloned()
        .map(|t| (t, WorkerAttribution::WorkflowRun))
        .or_else(|| thread_of_worker(world, &worker).map(|t| (t, WorkerAttribution::RecordAgentId)))
        .or_else(|| {
            meta.session
                .as_ref()
                .map(|s| ThreadId::of_session(s.clone()))
                .filter(|t| world.threads.contains_key(t))
                .map(|t| (t, WorkerAttribution::TranscriptFile))
        });
    let Some((thread, how)) = placed else {
        // All three rungs missed: no parent `Workflow` call, no transcript for
        // the worker, and no session on the record — which now means the
        // journal was read from somewhere other than a session directory, since
        // `polis_ingest::transcript::attribute_to_file` fills the session in
        // from the path for every line that came out of one. It is real and its
        // thread is not knowable, so it is parked rather than guessed, and
        // adopted later by run id if the parent call ever turns up.
        park_unattributed(
            world,
            &worker,
            Some(AgentType::new("workflow-subagent")),
            at,
            "workflow run has no parent `Workflow` call and no session on the record",
            Some(run),
        );
        return;
    };
    attach_worker(
        world,
        &thread,
        &worker,
        Some(AgentType::new("workflow-subagent")),
        at,
        how,
    );
    let finished = event.kind == TranscriptRecordKind::Result;
    if let Some(w) = world
        .threads
        .get_mut(&thread)
        .and_then(|t| t.worker_mut(&worker))
    {
        w.workflow_run = Some(run.clone());
        w.running = !finished;
        w.last_activity = at;
    }
    // A `started` with no matching `result` is a subagent still running, or
    // crashed — which is exactly what `running` now says.
}

// ---------------------------------------------------------------------------
// Polis's own health
// ---------------------------------------------------------------------------

/// Applies one control event.
pub(crate) fn control(world: &mut World, event: &ControlEvent) {
    match event {
        ControlEvent::ChannelDegraded { channel, reason } => {
            world
                .health
                .degraded
                .insert(channel.to_string(), reason.clone());
            if *channel == Channel::Otel {
                // Every tool call is now attributed to the main agent, and the
                // session says so rather than pretending otherwise (ADR-0006).
                world.health.subagent_attribution_degraded = true;
            }
        }
        ControlEvent::EventsDropped { count, channel } => {
            *world.health.dropped.entry(channel.to_string()).or_insert(0) += *count;
        }
        ControlEvent::SequenceGap { .. } => {
            world.health.sequence_gaps += 1;
        }
        ControlEvent::SchemaDrift {
            channel, detail, ..
        } => {
            tracing::debug!(%channel, %detail, "schema drift");
            world.health.drift += 1;
        }
        ControlEvent::SessionSuperseded { session, by } => {
            tracing::debug!(%session, %by, "conversation cleared; closing its thread");
            world.supersede_thread(&ThreadId::of_session(session.clone()));
        }
        ControlEvent::Shutdown => world.health.events_ignored += 1,
        // `ControlEvent` is `#[non_exhaustive]`: a new failure mode is itself a
        // drift signal rather than a compile break.
        _ => world.health.drift += 1,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Marks a thread as working, and records when.
///
/// A thread is [`ThreadStatus::Waiting`] exactly while it has a live
/// "needs decision" mark — one rule, in one place. Activity alone does not clear
/// the wait, because a thread parked on a permission prompt can still be
/// emitting telemetry; and resolving the mark is enough to end it, because the
/// mark *is* the wait.
///
/// The caller that proves that first clause is [`answered`]'s `tool_result` arm.
/// An orchestrator blocked on a question can have a dozen subagents returning
/// results a second, every one of them arriving here — and every one of them
/// re-deriving the wait from the attention layer rather than overwriting it, so
/// the thread stays [`ThreadStatus::Waiting`] while its clock advances. That is
/// what lets that arm stamp activity without also calling
/// [`World::resolve_decisions`].
fn working(world: &mut World, thread: &ThreadId, at: Instant) {
    if let Some(t) = world.threads.get_mut(thread) {
        t.last_activity = at;
    }
    derive(world, thread);
}

/// The main agent picked its turn back up.
///
/// Separate from [`working`] because `working` stamps for a **worker's** records
/// too — deliberately, so a live orchestrator is not retired out from under a
/// working fleet — and a subagent churning away says nothing about the main
/// agent's turn. Calling this from `working` made a fleet of subagents clear
/// their parent's `turn_ended`, which put a finished thread back to `Working`
/// and then let the 60-second clock decay it to `Idle`.
///
/// The interrupt clears here for the same reason, and it has to clear
/// somewhere: measured on this machine a `<task-notification>` arrives 429 ms
/// after an interrupt and the thread carries straight on, so a sticky
/// `Interrupted` would hold a blocking pin over visibly working work.
fn resumed(world: &mut World, thread: &ThreadId, at: Instant) {
    if let Some(t) = world.threads.get_mut(thread) {
        t.turn_ended = false;
        t.interrupted_at = None;
        t.last_activity = at;
    }
    derive(world, thread);
}

/// The **only** function that assigns [`Thread::status`].
///
/// # Why status is derived rather than written
///
/// It used to be written in five places that did not agree, and the disagreement
/// was not theoretical. Two call sites resolved a decision without re-deriving
/// (`OtelEvent::ToolBlockedOnUserSpan`, and the interrupt branch of
/// [`answered`]), and [`World::tick`] decays only `Working` — so a thread could
/// sit in `Waiting` with zero live marks until it was retired half an hour
/// later. Every state added to the enum was another way to get permanently
/// stuck.
///
/// So the channels now write *ledgers* — the attention layer,
/// [`Thread::background`], [`Thread::interrupted_at`], [`Thread::turn_ended`],
/// `World::pending` — and this reads them in one fixed precedence:
///
/// 1. **`Done`** is sticky. A session that ended cannot be resurrected by a
///    transcript record that was merely slow to arrive.
/// 2. **`Waiting`** — a live decision mark. Somebody asked the operator a
///    question and it has not been answered.
/// 3. **`Interrupted`** — the operator pressed Esc and nothing has happened
///    since.
/// 4. **`Working`** — a tool call is in flight, or the turn has not ended.
/// 5. **`Parked`** — the turn ended but a background job is still open. Ranks
///    below `Working` because a thread that is *also* calling tools is working,
///    whatever else it has running.
/// 6. **`Ready`** — the turn ended with the ledgers empty.
/// 7. **`Idle`** — none of the above can be shown. The honest last resort.
fn derive(world: &mut World, thread: &ThreadId) {
    let blocked = world
        .attention
        .iter()
        .any(|m| m.kind.is_decision_for(thread));
    let in_flight = world.pending_for(thread);
    let Some(t) = world.threads.get_mut(thread) else {
        return;
    };
    if t.status == ThreadStatus::Done {
        return;
    }
    t.status = if blocked {
        ThreadStatus::Waiting
    } else if t.interrupted_at.is_some() {
        ThreadStatus::Interrupted
    } else if in_flight || !t.turn_ended {
        ThreadStatus::Working
    } else if !t.background.is_empty() {
        ThreadStatus::Parked
    } else {
        ThreadStatus::Ready
    };
}

/// Raises a "needs decision" mark and parks the thread on it.
///
/// # An authoritative channel supersedes a reconstruction
///
/// A live session tails Channel D as well as Channel B (PRD §4.4), so the
/// transcript-derived sources below fire *alongside* the hooks — the same wait
/// seen twice. PRD §11.2 draws this state as **a** standing pin above the
/// building or district, so it gets one pin: a reconstruction yields to a hook
/// on the same thread and never draws beside it, and a reconstruction replaces
/// an earlier reconstruction rather than stacking with it.
fn needs_decision(
    world: &mut World,
    thread: &ThreadId,
    source: DecisionSource,
    at: Instant,
    path: Option<LogicalPath>,
) {
    let reconstructed_for = |m: &crate::attention::Attention, want: bool| {
        matches!(
            &m.kind,
            AttentionKind::NeedsDecision { thread: t, source: s, .. }
                if t == thread && s.is_reconstructed() == want
        )
    };
    if source.is_reconstructed() {
        if world.attention.iter().any(|m| reconstructed_for(m, false)) {
            return;
        }
        world.attention.retain(|m| !reconstructed_for(m, true));
    } else {
        world.attention.retain(|m| !reconstructed_for(m, true));
    }
    if let Some(t) = world.threads.get_mut(thread) {
        t.status = ThreadStatus::Waiting;
        t.last_activity = at;
    }
    world.raise(
        AttentionKind::NeedsDecision {
            thread: thread.clone(),
            at: path,
            source,
        },
        at,
    );
}

// ---------------------------------------------------------------------------
// Channel D's reconstruction of PRD §11.2 state (a)
//
// A replayed transcript carries no hooks, so none of the four hook sources
// above can fire and the attention layer was — measured — 0.000% of map area in
// every frame of all three M2 recordings. The three rules below are what a
// transcript *does* carry. They are marked as reconstructed
// (`DecisionSource::is_reconstructed`) and documented in docs/replay/README.md,
// because PRD §17 requires anything that drives an alert to come from an
// authoritative channel and a recording is not one.
// ---------------------------------------------------------------------------

/// Raises and clears "needs decision" at an assistant turn boundary.
///
/// Two rules, both **prospective** — the mark's onset and duration are the real
/// ones, exactly as a `PermissionRequest` hook would have given them:
///
/// * a `tool_use` for a tool that *is* a question to the operator
///   ([`crate::attention::asks_the_operator`]) raises the mark, and its
///   `tool_result` — the answer — clears it on the thread's next call;
/// * a **main** agent ending its turn with no tool call at all is waiting on a
///   human, which is the transcript's form of the `idle_prompt` /
///   `agent_needs_input` notification PRD §11.2 lists under this state.
///
/// A main agent making a tool call is the thread making progress, which
/// [`World::resolve_decisions`] already treats as equivalent to the human having
/// answered. A **worker's** call is not: a fleet of subagents churning away says
/// nothing about whether the operator replied.
fn turn_boundary(
    world: &mut World,
    thread: &ThreadId,
    worker: Option<&WorkerId>,
    record: &ThreadedRecord,
    at: Instant,
) {
    let calls: Vec<&str> = blocks(record)
        .iter()
        .filter(|b| b.kind.as_deref() == Some("tool_use"))
        .filter_map(|b| b.name.as_deref())
        .collect();
    if !calls.is_empty() {
        if worker.is_none() {
            world.resolve_decisions(thread);
            resumed(world, thread, at);
        }
        for name in calls {
            if crate::attention::asks_the_operator(&ToolKind::parse(name)) {
                needs_decision(world, thread, DecisionSource::AskUser, at, None);
            }
        }
        return;
    }
    // `stop_reason` is the only field that says "this really was the end of the
    // turn". A text-only record with no stop reason is a streamed fragment, and
    // treating it as an idle boundary makes the pin flicker on every paragraph.
    //
    // An allowlist rather than a match on one value, with anything unrecognised
    // counted rather than guessed at: `max_tokens` (the answer was truncated)
    // and `pause_turn` ("not finished, call again") are documented boundaries
    // that do not appear in this machine's 52,528 recorded stop reasons, and a
    // silent mismatch would read as a thread that never finished a turn.
    let ended = match record
        .message
        .as_ref()
        .and_then(|m| m.stop_reason.as_deref())
    {
        Some("end_turn" | "stop_sequence") => true,
        Some("tool_use") | None => false,
        Some(_) => {
            world.health.events_ignored += 1;
            false
        }
    };
    if worker.is_none() && ended {
        // NOT a decision. This line used to raise
        // `DecisionSource::TurnEnded`, which painted the loudest state in the
        // product — amber, uppercase, sorted first, never decaying — on the
        // single most common thing an agent does. PRD §11.2 lists the sources of
        // *needs decision* as `PermissionRequest`, `Elicitation` and
        // `TeammateIdle`; a turn ending is not among them, and §11.2(b) files it
        // under `done` instead: "a main thread going idle is news".
        //
        // What the boundary means now is decided by `derive` from the ledgers:
        // `Ready` with nothing outstanding, `Parked` with a background job still
        // open, and still `Waiting` if a real question is pending.
        if let Some(t) = world.threads.get_mut(thread) {
            t.turn_ended = true;
            t.last_activity = at;
        }
        derive(world, thread);
    }
}

/// Reads a `user` record for the operator's answer.
///
/// The **retrospective** rule, and the only one: a `tool_result` carrying
/// `toolDenialKind`, an `interruptedMessageId`, or the literal
/// `[Request interrupted by user]` record is proof that a permission prompt was
/// shown *and answered*. Channel D has no record of the prompt itself, so this
/// mark arrives with the answer rather than with the question — late by however
/// long the operator took to decide ([`DecisionSource::onset_is_exact`] is
/// `false` for it).
///
/// Anything else that is a genuine human turn — not injected (`isMeta`), not a
/// tool result, not a subagent's — is the answer to whatever the thread was
/// waiting for.
///
/// # A `tool_result` is not an answer, but it *is* activity
///
/// Two different questions, and this function used to conflate them by returning
/// without touching the clock at all. A `tool_result` is the **only** record
/// Channel D emits when a long `Bash` call ends, and the gap between it and the
/// assistant record that follows is the model's own latency: measured over
/// 59 334 result→assistant pairs in this machine's transcripts, median 3.8 s,
/// p75 7.6 s, but **p99 69 s**, so on better than one turn in a hundred a thread
/// went [`crate::ThreadStatus::Idle`] while the model was mid-thought.
///
/// So the arm stamps [`working`] and returns *without*
/// [`World::resolve_decisions`]. The omission is the point: a tool coming back
/// says nothing about whether the human replied, so a result must never take a
/// pin down. `working` re-derives the wait from the attention layer on every
/// call, which is what makes stamping safe — a thread parked on a decision is
/// re-stamped [`crate::ThreadStatus::Waiting`], not `Working`.
///
/// It stamps regardless of `worker`, matching [`assistant`], which calls
/// `working` before it looks at one. **That moves the lifetime clocks, and
/// deliberately**: `working` sets `last_activity`, which
/// [`World::retire_threads`] and [`crate::MARK_HOLD_MAX`] read, so a thread
/// waiting on a decision whose subagents keep returning results now lives in the
/// world longer than it did. That is the honest reading — it *is* alive — and
/// the alternative, a clock only a main agent's own records advance, retires a
/// live orchestrator out from under a working fleet.
fn answered(
    world: &mut World,
    thread: &ThreadId,
    worker: Option<&WorkerId>,
    raw: &Value,
    record: &ThreadedRecord,
    at: Instant,
) {
    if interrupted(record) {
        // The operator pressed Esc. This used to fall into the branch below and
        // be thrown away — the line was literally `let _ = at;` — so the thread
        // kept whatever status it had, went silent, and 60 seconds later the
        // decay clock relabelled it `Idle`. That is the operator's own report:
        // *"i just interrupted this chat, the chat says 'idle', it should say
        // 'interrupted'"*.
        //
        // Both wordings land here. `[Request interrupted by user for tool use]`
        // is 41 of this machine's 86 interrupts and 15 of those are the last
        // record in their file; classifying it as a mere tool rejection — which
        // its neighbouring `user-rejected` tool_result makes tempting — leaves
        // half of all interrupts decaying into `Idle`.
        world.resolve_decisions(thread);
        if let Some(t) = world.threads.get_mut(thread) {
            t.interrupted_at = Some(at);
            t.last_activity = at;
            t.turn_ended = true;
        }
        derive(world, thread);
        return;
    }
    if denied(raw, record) {
        // A rejection is an ANSWER, not a question. `DecisionSource::Rejected`'s
        // own contract says its evidence "arrives with the operator's answer
        // rather than with the question" — so raising it as a pending decision
        // tells the operator they owe a reply to something they already
        // declined. On their live map it rendered as "WAITING ON YOU · you said
        // no", which is a contradiction in four words, and PRD §11.2a reserves
        // that state for a thread that is genuinely blocked on a human.
        //
        // Resolving instead: any decision this thread was waiting on is now
        // settled, so the pin comes down rather than a second one going up.
        world.resolve_decisions(thread);
        working(world, thread, at);
        return;
    }
    let is_tool_result = blocks(record)
        .iter()
        .any(|b| b.kind.as_deref() == Some("tool_result"));
    if is_tool_result {
        // Not the operator's answer — but proof the thread was alive at `at`.
        // See the section above for why this deliberately does not resolve.
        working(world, thread, at);
        return;
    }
    if worker.is_some() || record.is_meta == Some(true) {
        return;
    }
    // A machine waking the thread is not the operator answering it.
    //
    // `<task-notification>` records — a background shell reporting in, a
    // subagent finishing — arrive as ordinary `user` records and were, until
    // here, indistinguishable from the human typing. 218 of them in this
    // machine's corpus were being read as "the operator replied", which took
    // down pins nobody had answered and is the second half of the report *"then
    // it starts again automatically and the 'waiting' disappears"*.
    //
    // `origin.kind` names the difference, and it is absent on ~30% of genuine
    // human prompts, so the test is one-sided on purpose: only an explicit
    // `task-notification` is treated as machine. Anything else — including a
    // missing `origin` — stays the operator, which keeps today's behaviour as
    // the fallback rather than silently withholding pins from real answers.
    let machine_wake = record
        .origin
        .as_ref()
        .and_then(|o| o.get("kind"))
        .and_then(Value::as_str)
        == Some("task-notification");
    if machine_wake {
        // It is activity, and it closes whatever background job just reported —
        // but it answers nothing.
        close_background(world, thread, raw);
        resumed(world, thread, at);
        return;
    }
    world.resolve_decisions(thread);
    resumed(world, thread, at);
}

/// Opens a background job on the `tool_result` that launched it.
///
/// `Bash(run_in_background: true)` answers in milliseconds and then runs for
/// minutes, so this is the only moment Polis learns the job exists. Two shapes
/// carry it, measured across this machine's 125 transcripts that have any:
/// `toolUseResult.backgroundTaskId` (360 distinct ids), and the result text
/// `Command running in background with ID: …` for the launches that predate the
/// structured field.
fn open_background(
    world: &mut World,
    thread: &ThreadId,
    tool_use: &str,
    sidecar: &Value,
    at: Instant,
) {
    let id = sidecar
        .get("backgroundTaskId")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            let text = sidecar.get("stdout").and_then(Value::as_str)?;
            text.split_once("Command running in background with ID:")
                .map(|(_, rest)| rest.trim().lines().next().unwrap_or("").trim().to_owned())
                .filter(|id| !id.is_empty())
        });
    let Some(id) = id else { return };
    let Some(t) = world.threads.get_mut(thread) else {
        return;
    };
    if t.background.iter().any(|b| b.id == id) {
        return;
    }
    t.background.push(crate::BackgroundTask {
        id,
        tool_use: Some(tool_use.to_owned()),
        label: sidecar
            .get("command")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        since: at,
    });
}

/// Closes the background job a `<task-notification>` reports on.
///
/// The notification names its `<task-id>`, so the close is keyed rather than
/// guessed. Its own payload warns that "the same task-id may notify more than
/// once", so removing an id that is already gone is a no-op by construction.
///
/// A job whose notification never arrives — measured at 17 of 357, ~5% — is
/// reaped by [`expire_background`] instead.
fn close_background(world: &mut World, thread: &ThreadId, raw: &Value) {
    let text = raw
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            let blocks = raw.get("message")?.get("content")?.as_array()?;
            blocks
                .iter()
                .find_map(|b| b.get("text").and_then(Value::as_str))
                .map(ToOwned::to_owned)
        });
    let Some(text) = text else { return };
    let Some(id) = text
        .split_once("<task-id>")
        .and_then(|(_, rest)| rest.split_once("</task-id>"))
        .map(|(id, _)| id.trim().to_owned())
    else {
        return;
    };
    if let Some(t) = world.threads.get_mut(thread) {
        t.background.retain(|b| b.id != id);
    }
}

/// Whether a `user` record proves the operator pressed Esc.
///
/// # Both wordings, and why the second one is not a rejection
///
/// Claude Code writes one of two text blocks, and this machine's corpus splits
/// them 45 / 41:
///
/// * `[Request interrupted by user]` — Esc at the prompt, mid-thought;
/// * `[Request interrupted by user for tool use]` — Esc while a tool call was
///   up, which is **48% of all interrupts**, and 15 of those 41 are the last
///   record in their file.
///
/// The second is easy to mistake for a permission denial, because a
/// `user-rejected` `tool_result` sits immediately before it (verified on 4 of 4
/// sampled contexts). It is not: `toolDenialKind` is present on **0 of 41** of
/// these records, the denial belongs to the *previous* record, and the operator
/// who hit Esc has stopped the thread rather than answered it. Calling it a
/// rejection leaves half of all interrupts decaying into `Idle`, which is the
/// bug this predicate exists to close.
///
/// Matched on the exact stripped text rather than a prefix, so that a tool
/// result quoting the phrase — this repository's own source does — cannot
/// masquerade as one.
fn interrupted(record: &ThreadedRecord) -> bool {
    blocks(record).iter().any(|b| {
        b.kind.as_deref() == Some("text")
            && b.text.as_deref().is_some_and(|t| {
                let t = t.trim();
                t == "[Request interrupted by user]"
                    || t == "[Request interrupted by user for tool use]"
            })
    })
}

/// Whether a `user` record proves the operator was asked and said no.
///
/// `toolDenialKind` (`user-rejected` | `permission-rule` | `automode-blocked` |
/// `automode-unavailable`) is a raw envelope field that `ThreadedRecord` does
/// not model, so it is read off the verbatim record — which PRD §4.4's
/// `{ known fields } + Value` rule keeps available exactly for cases like this.
///
/// `interruptedMessageId` used to be read here too. It moved to
/// [`interrupted`]: it rides on 51 of 86 interrupt records and means Esc, not a
/// denial, so answering it as a rejection lost the state entirely.
fn denied(raw: &Value, _record: &ThreadedRecord) -> bool {
    raw.get("toolDenialKind").is_some()
}

/// Finishes a thread, raising PRD §11.2's `done` split by verification.
///
/// **Main agents only.** `SubagentStop` is noise; a main thread going idle is
/// news.
fn finish_thread(world: &mut World, thread: &ThreadId, at: Instant, mark: bool) {
    let verified = world
        .threads
        .get(thread)
        .is_some_and(|t| t.is_verified(&world.files));
    if let Some(t) = world.threads.get_mut(thread) {
        t.status = ThreadStatus::Done;
        t.last_activity = at;
    }
    world.resolve_decisions(thread);
    if mark {
        world.raise(
            AttentionKind::Done {
                thread: thread.clone(),
                verified,
            },
            at,
        );
    }
}

/// Attaches a worker to a thread, or parks it when there is no thread.
fn attach_worker(
    world: &mut World,
    thread: &ThreadId,
    worker: &WorkerId,
    kind: Option<AgentType>,
    at: Instant,
    evidence: WorkerAttribution,
) {
    if !world.threads.contains_key(thread) {
        park_unattributed(
            world,
            worker,
            kind,
            at,
            "no thread for this worker's session",
            None,
        );
        return;
    }
    if world.unattributed.remove(worker).is_some() {
        count_unattributed(world);
    }
    let Some(t) = world.threads.get_mut(thread) else {
        return;
    };
    let w = t.worker_entry(worker, at);
    w.last_activity = at;
    w.upgrade_attribution(evidence);
    if let Some(kind) = kind {
        if !kind.is_empty() {
            w.kind = kind;
        }
    }
}

/// Marks a worker finished. Never raises attention: `SubagentStop` is noise.
fn finish_worker(
    world: &mut World,
    thread: &ThreadId,
    worker: &WorkerId,
    kind: Option<AgentType>,
    at: Instant,
) {
    attach_worker(
        world,
        thread,
        worker,
        kind,
        at,
        WorkerAttribution::RecordAgentId,
    );
    if let Some(w) = world
        .threads
        .get_mut(thread)
        .and_then(|t| t.worker_mut(worker))
    {
        w.running = false;
        w.last_activity = at;
    }
}

/// Records a worker Polis cannot place.
fn park_unattributed(
    world: &mut World,
    worker: &WorkerId,
    kind: Option<AgentType>,
    at: Instant,
    reason: &'static str,
    workflow_run: Option<&str>,
) {
    let entry = world
        .unattributed
        .entry(worker.clone())
        .or_insert_with(|| UnattributedWorker {
            id: worker.clone(),
            kind: None,
            focus: None,
            first_seen: at,
            last_seen: at,
            records: 0,
            workflow_run: None,
            reason,
        });
    entry.last_seen = at;
    entry.records = entry.records.saturating_add(1);
    if kind.is_some() {
        entry.kind = kind;
    }
    if workflow_run.is_some() {
        entry.workflow_run = workflow_run.map(ToOwned::to_owned);
    }
    count_unattributed(world);
}

/// The run id, without the `wf_` prefix.
///
/// **The two sources spell it differently**, and the difference orphans every
/// workflow subagent if it is missed: `toolUseResult.runId` is
/// `wf_03be0d34-d97`, while `polis_events::TranscriptSource::Subagent`'s
/// `workflow_run` comes from the `wf_<runId>` **directory name** and has already
/// had the prefix stripped. Verified against a live session with eleven workflow
/// runs, where keying the table on the raw `runId` matched none of them.
fn workflow_run_key(raw: &str) -> &str {
    raw.strip_prefix("wf_").unwrap_or(raw)
}

/// The thread a worker is already known to belong to.
///
/// A subagent's own transcript carries the parent's `sessionId`, so by the time
/// a `journal.jsonl` line for it arrives the worker is usually already attached.
/// Checking here is what stops the journal — which is read last, because it has
/// no timestamps to sort by — from parking a worker that is already placed.
fn thread_of_worker(world: &World, worker: &WorkerId) -> Option<ThreadId> {
    world
        .threads
        .iter()
        .find(|(_, t)| t.worker(worker).is_some())
        .map(|(id, _)| id.clone())
}

/// Keeps [`crate::Health::unattributed_workers`] equal to how many workers are
/// unplaced **now**, not to how many ever were.
fn count_unattributed(world: &mut World) {
    world.health.unattributed_workers = u64::try_from(world.unattributed.len()).unwrap_or(u64::MAX);
}

/// Adopts every parked worker belonging to a workflow run whose parent has just
/// been identified.
///
/// Workflow subagents — 503 of 643 — have no `toolUseId` and no
/// `parentAgentId`; the run id matched against the parent `Workflow` call is
/// their only link (ADR-0013 route 3). The journal that names them can arrive
/// before that call does, so parking and adopting is what makes the attribution
/// order-independent rather than order-lucky.
fn adopt_workflow_run(world: &mut World, run: &str, thread: &ThreadId, at: Instant) {
    let adoptable: Vec<(WorkerId, Option<AgentType>)> = world
        .unattributed
        .values()
        .filter(|w| w.workflow_run.as_deref() == Some(run))
        .map(|w| (w.id.clone(), w.kind.clone()))
        .collect();
    for (id, kind) in adoptable {
        attach_worker(world, thread, &id, kind, at, WorkerAttribution::WorkflowRun);
        if let Some(w) = world
            .threads
            .get_mut(thread)
            .and_then(|t| t.worker_mut(&id))
        {
            w.workflow_run = Some(run.to_owned());
        }
    }
}

/// Marks every file the thread wrote as verified as of `at`.
///
/// The honest approximation of PRD §11.2's "tests ran against the changed files
/// after the change": the channels carry a command line, not a coverage map.
fn mark_verified(world: &mut World, thread: &ThreadId, at: Instant) {
    let written: Vec<LogicalPath> = world
        .threads
        .get(thread)
        .map(|t| {
            t.visits
                .iter()
                .filter(|(_, v)| v.writes > 0)
                .map(|(p, _)| p.clone())
                .collect()
        })
        .unwrap_or_default();
    for path in written {
        world.file_entry(&path).last_verified = Some(at);
    }
    if let Some(t) = world.threads.get_mut(thread) {
        t.last_verified = Some(at);
    }
}

/// Registers a claim and counts the degradation when it has no line range.
fn register_claim(world: &mut World, claim: Claim) {
    let degraded = claim.lines.is_none();
    if world.claims.claim(claim).is_some() && degraded {
        world.health.contention_without_line_ranges += 1;
    }
}

/// Records the paths a shell command names as territory evidence (PRD §6.1).
///
/// Weighted at [`crate::shell::SHELL_ARGUMENT_WEIGHT`] and capped per command;
/// everything that makes this safe is in [`crate::shell`]'s module docs.
fn shell_evidence(
    world: &mut World,
    thread: &ThreadId,
    worker: Option<&WorkerId>,
    tool: &ToolKind,
    cwd: Option<&str>,
    command: &str,
    at: Instant,
) {
    if !tool.is_shell() {
        return;
    }
    for (path, scope) in world.shell_evidence(cwd, command) {
        world.observe(&Observation {
            thread: thread.clone(),
            worker: worker.cloned(),
            path,
            tool: tool.clone(),
            scope,
            at,
            weight: Some(crate::shell::SHELL_ARGUMENT_WEIGHT),
        });
    }
}

/// Records one observation for territory inference and the trail.
fn observe(
    world: &mut World,
    thread: &ThreadId,
    worker: Option<&WorkerId>,
    tool: &ToolKind,
    scope: PathScope,
    path: &LogicalPath,
    at: Instant,
) {
    world.observe(&Observation {
        thread: thread.clone(),
        worker: worker.cloned(),
        path: path.clone(),
        tool: tool.clone(),
        scope,
        at,
        weight: None,
    });
}

/// Updates a file's counters for one tool call.
fn record_file(
    world: &mut World,
    thread: &ThreadId,
    tool: &ToolKind,
    path: &LogicalPath,
    at: Instant,
) {
    let mutating = tool.is_mutating();
    let file = world.file_entry(path);
    if mutating {
        file.writes = file.writes.saturating_add(1);
        file.touch(thread, at);
    } else {
        file.reads = file.reads.saturating_add(1);
    }
    if mutating {
        if let Some(v) = world
            .threads
            .get_mut(thread)
            .and_then(|t| t.visits.get_mut(path))
        {
            v.writes = v.writes.saturating_add(1);
        }
    }
}

/// Records a path observation from a channel that already normalised it.
fn record_path(
    world: &mut World,
    thread: &ThreadId,
    worker: Option<&WorkerId>,
    tool: &ToolKind,
    _worktree: WorktreeId,
    path: &LogicalPath,
    at: Instant,
) {
    observe(
        world,
        thread,
        worker,
        tool,
        PathScope::for_tool(tool),
        path,
        at,
    );
    record_file(world, thread, tool, path, at);
}

/// Registers a working directory against a thread.
fn note_cwd(world: &mut World, thread: &ThreadId, cwd: &str) {
    let worktree = world.note_cwd(cwd);
    if let Some(t) = world.threads.get_mut(thread) {
        if t.cwd.as_deref() != Some(cwd) {
            t.cwd = Some(cwd.to_owned());
        }
        if worktree.is_some() {
            t.worktree = worktree;
        }
    }
}

/// Remembers an in-flight call, evicting the oldest if the table is full.
fn insert_pending(world: &mut World, id: ToolUseId, call: PendingCall) {
    if world.pending.len() >= PENDING_CAP {
        if let Some(oldest) = world
            .pending
            .iter()
            .min_by_key(|(_, c)| c.started)
            .map(|(k, _)| k.clone())
        {
            world.pending.remove(&oldest);
        }
    }
    world.pending.insert(id, call);
}

/// The blocks of a threaded record, or an empty slice.
fn blocks(record: &ThreadedRecord) -> &[Block] {
    match record.message.as_ref().and_then(|m| m.content.as_ref()) {
        Some(Content::Blocks(b)) => b,
        _ => &[],
    }
}

/// Every path a tool input names, with the scope it claims.
///
/// **Never `unwrap` a key**: `Read.file_path` is absent on 9 records of 5 844,
/// replaced by `__unparsedToolInput` when the model emitted malformed JSON for
/// the tool input and the harness kept it verbatim.
fn tool_input_paths(
    tool: &ToolKind,
    input: Option<&Value>,
    cwd: Option<&str>,
) -> Vec<(String, PathScope, PathOrigin)> {
    let mut out = Vec::new();
    if let Some(input) = input {
        for key in ["file_path", "notebook_path", "filename"] {
            if let Some(v) = input.get(key).and_then(Value::as_str) {
                out.push((v.to_owned(), PathScope::File, PathOrigin::Input));
            }
        }
        if let Some(v) = input.get("path").and_then(Value::as_str) {
            out.push((v.to_owned(), PathScope::for_tool(tool), PathOrigin::Input));
        }
    }
    // A shell call's only path evidence is its working directory, and it is
    // noisy — weight 0.5 in PRD §6.1's table.
    if out.is_empty() && tool.is_shell() {
        if let Some(cwd) = cwd {
            out.push((cwd.to_owned(), PathScope::Directory, PathOrigin::Cwd));
        }
    }
    out
}

/// The agent's own summary of a call, vetted, or nothing.
///
/// # This is the whole door
///
/// [`crate::Intent`] explains why the world holds this at all. This function is
/// the only thing that fills it, and it reads exactly one key — `description` —
/// out of a tool input that also contains `command`, `content`, `old_string`
/// and `prompt`. A future key is a deliberate edit here, not an oversight
/// somewhere else.
///
/// # It is vetted on the way in, not on the way out
///
/// [`polis_repo::llm::outbound::vet`] is the gate ADR-0089 built for text
/// leaving the machine, and it is applied *here*, at capture. The stricter
/// placement is the point: a caption is written from these lines and the world
/// is snapshotted, cloned and drawn, so a credential-shaped `description` that
/// lived in the ring would be one bug away from a screenshot as well as one bug
/// away from a request. Refusing it at the door means no copy of it exists.
///
/// The cost of a false positive is one missing sentence out of twenty, which
/// the caption will not notice.
fn intent_of(tool: &ToolKind, input: Option<&Value>, at: Instant) -> Option<crate::Intent> {
    let raw = input?.get("description")?.as_str()?;
    // The report is discarded: `RedactionReport` names paths, and there is no
    // path here to name. What matters is the `Err`, which is the refusal.
    let mut report = polis_repo::llm::outbound::RedactionReport::default();
    let mut text = polis_repo::llm::outbound::vet(raw, &mut report).ok()?;
    if text.chars().count() > crate::INTENT_TEXT_MAX {
        let cut = text
            .char_indices()
            .nth(crate::INTENT_TEXT_MAX)
            .map_or(text.len(), |(i, _)| i);
        text.truncate(cut);
    }
    (!text.is_empty()).then(|| crate::Intent {
        tool: tool.clone(),
        text,
        at,
    })
}

/// Whether a path came from the tool's own input or from the shell fallback.
///
/// The distinction is the difference between rung 1 and rung 2 of
/// [`crate::place`]'s chain, and it is not recoverable after the fact: both
/// arrive as a [`LogicalPath`] and a [`PathScope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathOrigin {
    /// `file_path`, `notebook_path`, `filename` or `path`.
    Input,
    /// The record envelope's working directory, for a shell call that named
    /// nothing.
    Cwd,
}

/// Approximates a line delta from an edit's own strings.
///
/// The fallback for the 65% of subagent tool results with no `structuredPatch`.
/// `polis-ingest` elides string leaves over 2 048 characters and writes the
/// original line count into the marker, so an elided `content` still yields an
/// exact line count — which is why this reads the marker rather than only
/// counting newlines.
fn approximate_diff(tool: &ToolKind, input: Option<&Value>) -> Option<(u32, u32)> {
    let input = input?;
    match tool {
        ToolKind::Edit => {
            let removed = input
                .get("old_string")
                .and_then(Value::as_str)
                .map_or(0, line_count);
            let added = input
                .get("new_string")
                .and_then(Value::as_str)
                .map_or(0, line_count);
            Some((added, removed))
        }
        ToolKind::Write | ToolKind::NotebookEdit => {
            let added = input
                .get("content")
                .or_else(|| input.get("new_source"))
                .and_then(Value::as_str)
                .map_or(0, line_count);
            Some((added, 0))
        }
        _ => None,
    }
}

/// Lines in a string, honouring `polis-ingest`'s elision marker.
fn line_count(text: &str) -> u32 {
    if let Some(marker) = text.rfind("…[") {
        // `…[N chars, M lines elided]` — M is the *original* line count.
        let tail = &text[marker..];
        if let Some(rest) = tail.split(", ").nth(1) {
            if let Some(lines) = rest.split(' ').next().and_then(|n| n.parse::<u32>().ok()) {
                return lines;
            }
        }
    }
    u32::try_from(text.lines().count()).unwrap_or(u32::MAX)
}

/// Sums a `structuredPatch` into `(added, removed, (first_line, last_line))`.
///
/// Every entry in a hunk's `lines[]` keeps its unified-diff prefix, and
/// `newStart..newStart+newLines` is the range PRD §11.3's overlap test needs.
fn patch_totals(patch: &Value) -> Option<(u32, u32, (u32, u32))> {
    let hunks = patch.as_array()?;
    if hunks.is_empty() {
        return None;
    }
    let mut added = 0_u32;
    let mut removed = 0_u32;
    let mut lo = u32::MAX;
    let mut hi = 0_u32;
    for hunk in hunks {
        if let Some(lines) = hunk.get("lines").and_then(Value::as_array) {
            for line in lines {
                match line.as_str().and_then(|s| s.as_bytes().first()) {
                    Some(b'+') => added = added.saturating_add(1),
                    Some(b'-') => removed = removed.saturating_add(1),
                    _ => {}
                }
            }
        }
        let start = hunk
            .get("newStart")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0);
        let len = hunk
            .get("newLines")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0);
        lo = lo.min(start);
        hi = hi.max(start.saturating_add(len.saturating_sub(1)));
    }
    if lo == u32::MAX {
        lo = 0;
    }
    Some((added, removed, (lo, hi.max(lo))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_elided_string_still_yields_its_original_line_count() {
        // `polis-ingest` cuts string leaves at 2 048 characters and writes the
        // original line count into the marker, which is the only reason a
        // sidecar-less `Write` can report a height at all.
        let elided = "fn main() {}…[9000 chars, 412 lines elided]";
        assert_eq!(line_count(elided), 412);
        assert_eq!(line_count("a\nb\nc"), 3);
        assert_eq!(line_count(""), 0);
    }

    #[test]
    fn a_structured_patch_gives_exact_counts_and_a_line_range() {
        let patch = serde_json::json!([
            {
                "oldStart": 10, "oldLines": 3, "newStart": 10, "newLines": 4,
                "lines": [" ctx", "-gone", "+new", "+also", " ctx"]
            },
            {
                "oldStart": 40, "oldLines": 1, "newStart": 41, "newLines": 1,
                "lines": ["-a", "+b"]
            }
        ]);
        let (added, removed, range) = patch_totals(&patch).expect("a patch");
        assert_eq!((added, removed), (3, 2));
        assert_eq!(range, (10, 41));
        assert!(patch_totals(&serde_json::json!([])).is_none());
        assert!(patch_totals(&serde_json::json!("error string")).is_none());
    }

    /// The door is one key wide. Everything else in a tool input is the work
    /// itself, and the whole argument for `Intent` being safe rests on this
    /// function reading `description` and nothing beside it.
    #[test]
    fn only_the_description_gets_through_the_intent_door() {
        let now = Instant::now();
        let input = serde_json::json!({
            "command": "curl -H 'x-api-key: sk-live-9f2b' https://example.test",
            "description": "Check the workspace tests",
            "content": "fn main() { let password = \"hunter2\"; }",
            "old_string": "before",
            "prompt": "the operator's own words",
        });
        let intent = intent_of(&ToolKind::Bash, Some(&input), now).expect("a description");
        assert_eq!(intent.text, "Check the workspace tests");
        assert_eq!(intent.tool, ToolKind::Bash);
        // A tool that carries no description contributes nothing, which is most
        // of them: `Read`, `Grep`, `Edit`.
        let bare = serde_json::json!({"file_path": "src/auth/token.rs"});
        assert!(intent_of(&ToolKind::Read, Some(&bare), now).is_none());
        assert!(intent_of(&ToolKind::Read, None, now).is_none());
    }

    /// Vetting happens at capture, so a credential-shaped sentence never
    /// reaches the ring — not the snapshot, not a caption, not a screenshot.
    #[test]
    fn a_credential_shaped_description_is_refused_at_the_door() {
        let now = Instant::now();
        let leaky = serde_json::json!({
            "description": "Set ANTHROPIC_API_KEY=sk-ant-api03-Zx9Q2mVn8LpR4TdW6YbK1JhF7CsG",
        });
        assert!(
            intent_of(&ToolKind::Bash, Some(&leaky), now).is_none(),
            "a secret in a description is a secret"
        );
        // Control characters and bidi overrides are stripped rather than
        // refused: they are a spoofing channel, not a credential. Runs of
        // ordinary whitespace collapse to one space.
        let spoof = serde_json::json!({"description": "  Run \u{202E} the   tests  "});
        let intent = intent_of(&ToolKind::Bash, Some(&spoof), now).expect("still a sentence");
        assert_eq!(intent.text, "Run the tests");
    }

    /// A retried command writes the same sentence again, and three copies of
    /// one line would crowd out the three different things before it.
    #[test]
    fn the_intent_ring_is_bounded_and_drops_an_immediate_repeat() {
        let now = Instant::now();
        let mut thread = crate::Thread::new(
            ThreadId::of_session(polis_events::SessionId::new("intents")),
            polis_events::SessionId::new("intents"),
            now,
        );
        for _ in 0..3 {
            thread.push_intent(crate::Intent {
                tool: ToolKind::Bash,
                text: "Run the workspace tests".to_owned(),
                at: now,
            });
        }
        assert_eq!(thread.intents.len(), 1, "a repeat refreshes nothing");
        for i in 0..(crate::INTENT_CAP * 2) {
            thread.push_intent(crate::Intent {
                tool: ToolKind::Bash,
                text: format!("step {i}"),
                at: now,
            });
        }
        assert_eq!(thread.intents.len(), crate::INTENT_CAP);
        assert_eq!(
            thread.intents.back().expect("newest").text,
            format!("step {}", crate::INTENT_CAP * 2 - 1),
            "newest last"
        );
    }

    #[test]
    fn a_malformed_tool_input_yields_no_path_instead_of_panicking() {
        // 9 of 5 844 `Read` calls carry `__unparsedToolInput` and no `file_path`.
        let input = serde_json::json!({"__unparsedToolInput": "{file_path: ..."});
        assert!(tool_input_paths(&ToolKind::Read, Some(&input), None).is_empty());
        assert!(tool_input_paths(&ToolKind::Read, None, None).is_empty());
    }

    #[test]
    fn a_shell_call_falls_back_to_its_cwd_as_directory_evidence() {
        let input = serde_json::json!({"command": "cargo test"});
        let paths = tool_input_paths(&ToolKind::PowerShell, Some(&input), Some("C:/repo"));
        assert_eq!(
            paths,
            vec![("C:/repo".to_owned(), PathScope::Directory, PathOrigin::Cwd)]
        );
        // With no cwd there is nothing to say.
        assert!(tool_input_paths(&ToolKind::Bash, Some(&input), None).is_empty());
    }

    #[test]
    fn grep_claims_a_directory_and_read_claims_a_file() {
        let grep = serde_json::json!({"pattern": "x", "path": "src/auth"});
        assert_eq!(
            tool_input_paths(&ToolKind::Grep, Some(&grep), None),
            vec![(
                "src/auth".to_owned(),
                PathScope::Directory,
                PathOrigin::Input
            )]
        );
        let read = serde_json::json!({"file_path": "C:/repo/src/auth/token.rs"});
        assert_eq!(
            tool_input_paths(&ToolKind::Read, Some(&read), None),
            vec![(
                "C:/repo/src/auth/token.rs".to_owned(),
                PathScope::File,
                PathOrigin::Input
            )]
        );
    }

    #[test]
    fn an_edit_with_no_sidecar_still_approximates_its_diff() {
        let input = serde_json::json!({
            "file_path": "x.rs",
            "old_string": "a\nb",
            "new_string": "a\nb\nc\nd",
        });
        assert_eq!(
            approximate_diff(&ToolKind::Edit, Some(&input)),
            Some((4, 2))
        );
        let write = serde_json::json!({"file_path": "x.rs", "content": "1\n2\n3"});
        assert_eq!(
            approximate_diff(&ToolKind::Write, Some(&write)),
            Some((3, 0))
        );
        assert!(approximate_diff(&ToolKind::Read, Some(&write)).is_none());
    }
}
