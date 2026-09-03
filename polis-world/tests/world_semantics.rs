//! The semantics `World::apply` promises, driven one event at a time.
//!
//! The transcript path is covered against real data in `real_corpus.rs` and
//! `fixture_session.rs`. This file covers the three channels a *live* session
//! adds — hooks, OTel and the filesystem — plus the attention and contention
//! rules that only a live fleet produces, because no single recorded session
//! contains two threads colliding on one file.

use std::time::{Duration, Instant};

use polis_events::{
    Channel, ControlEvent, Event, EventKind, EventMeta, FsEvent, HookEvent, HookPayload,
    LogicalPath, OtelEvent, Outcome, Payload, SessionId, ThreadId, ToolCall, ToolKind, ToolUseId,
    WorkerId, WorktreeId,
};
use polis_layout::CityLayout;
use polis_world::attention::{AttentionKind, DecisionSource};
use polis_world::contention::{ContentionPrecision, Severity};
use polis_world::{ThreadStatus, World, MARK_HOLD_MAX};

const REPO: &str = "C:/repo";

fn world() -> World {
    let mut w = World::for_replay(CityLayout::default());
    w.mapper_mut()
        .add_worktree(WorktreeId::PRIMARY, std::path::Path::new(REPO))
        .expect("primary root");
    w
}

fn thread(session: &str) -> ThreadId {
    ThreadId::of_session(SessionId::new(session))
}

/// One hook datagram, as it arrives on the bus.
fn hook(session: &str, kind: EventKind, at: Instant, mut payload: HookPayload) -> Event {
    payload.session_id = Some(SessionId::new(session));
    payload.hook_event_name = Some(hook_name(kind).to_owned());
    payload.cwd = Some(REPO.to_owned());
    let mut meta = EventMeta::now(Channel::Hook).with_session(SessionId::new(session));
    meta.observed = at;
    meta.worker.clone_from(&payload.agent_id);
    Event::new(
        meta,
        Payload::Hook(Box::new(HookEvent {
            kind,
            truncated: false,
            payload,
        })),
    )
}

fn hook_name(kind: EventKind) -> &'static str {
    match kind {
        EventKind::PreToolUse => "PreToolUse",
        EventKind::PermissionRequest => "PermissionRequest",
        EventKind::Stop => "Stop",
        EventKind::SessionStart => "SessionStart",
        EventKind::SubagentStop => "SubagentStop",
        EventKind::Notification => "Notification",
        EventKind::ElicitationResult => "ElicitationResult",
        other => panic!("add a name for {other:?}"),
    }
}

/// A `PreToolUse` claim on one file.
fn pre_edit(session: &str, file: &str, branch: &str, at: Instant) -> Event {
    let mut payload = HookPayload {
        tool_name: Some("Edit".to_owned()),
        tool_input: Some(serde_json::json!({ "file_path": format!("{REPO}/{file}") })),
        ..HookPayload::default()
    };
    payload
        .extra
        .insert("gitBranch".to_owned(), serde_json::json!(branch));
    hook(session, EventKind::PreToolUse, at, payload)
}

/// An OTel `tool_result` for a settled call.
fn tool_result(
    session: &str,
    worker: Option<&str>,
    tool: ToolKind,
    file: &str,
    outcome: Outcome,
    at: Instant,
) -> Event {
    let mut meta = EventMeta::now(Channel::Otel).with_session(SessionId::new(session));
    meta.observed = at;
    meta.worker = worker.map(WorkerId::new);
    let call = ToolCall {
        tool,
        tool_use_id: Some(ToolUseId::new("toolu_1")),
        paths: vec![(WorktreeId::PRIMARY, LogicalPath::new(file).unwrap())],
        outcome,
        duration_ms: Some(4.0),
    };
    Event::new(
        meta,
        Payload::Otel(Box::new(OtelEvent::ToolResult(Box::new(call)))),
    )
}

#[test]
fn two_threads_editing_one_file_is_contention_and_it_says_how_sure_it_is() {
    let mut w = world();
    let t0 = Instant::now();
    w.apply(&pre_edit("a", "src/auth.ts", "main", t0));
    w.apply(&pre_edit(
        "b",
        "src/auth.ts",
        "main",
        t0 + Duration::from_secs(1),
    ));
    w.tick(t0 + Duration::from_secs(1));

    let marks: Vec<&AttentionKind> = w.attention.iter().map(|m| &m.kind).collect();
    assert_eq!(marks.len(), 1, "one relation, one link: {marks:?}");
    let AttentionKind::Contention(hit) = marks[0] else {
        panic!("expected contention, got {marks:?}");
    };
    assert_eq!(hit.severity, Severity::High, "same branch, same file");
    assert_eq!(
        hit.precision,
        ContentionPrecision::FileLevel,
        "the hook carries no line range, and the mark must say so (ADR-0004)"
    );
    assert_eq!(hit.path().as_str(), "src/auth.ts");
    assert_eq!(hit.threads(), (&thread("a"), &thread("b")));
    assert!(w.health.contention_without_line_ranges > 0);
}

#[test]
fn two_worktrees_on_one_logical_file_are_a_merge_conflict_you_will_meet_later() {
    // PRD §7.6: the same logical file in two physical places, which is exactly
    // what the shared base map exists to show.
    let mut w = world();
    w.mapper_mut()
        .add_worktree(WorktreeId(3), std::path::Path::new("C:/repo-wt-3"))
        .expect("worktree");
    let t0 = Instant::now();

    let mut payload = HookPayload {
        tool_name: Some("Edit".to_owned()),
        tool_input: Some(serde_json::json!({ "file_path": "C:/repo-wt-3/src/auth.ts" })),
        cwd: Some("C:/repo-wt-3".to_owned()),
        ..HookPayload::default()
    };
    payload
        .extra
        .insert("gitBranch".to_owned(), serde_json::json!("feature"));
    let mut event = hook(
        "b",
        EventKind::PreToolUse,
        t0 + Duration::from_secs(1),
        payload,
    );
    // `hook` overwrites cwd; put the worktree back.
    if let Payload::Hook(h) = &mut event.payload {
        h.payload.cwd = Some("C:/repo-wt-3".to_owned());
    }

    w.apply(&pre_edit("a", "src/auth.ts", "main", t0));
    w.apply(&event);
    w.tick(t0 + Duration::from_secs(1));

    let AttentionKind::Contention(hit) = &w.attention[0].kind else {
        panic!("expected contention");
    };
    assert_eq!(hit.severity, Severity::Medium);
    assert_eq!(
        hit.path().as_str(),
        "src/auth.ts",
        "one logical file, two checkouts — one key"
    );
}

#[test]
fn a_claim_expires_at_the_ttl_and_the_link_goes_with_it() {
    let mut w = world();
    let t0 = Instant::now();
    w.apply(&pre_edit("a", "src/auth.ts", "main", t0));
    w.apply(&pre_edit("b", "src/auth.ts", "main", t0));
    w.tick(t0);
    assert_eq!(w.attention.len(), 1);

    w.tick(t0 + polis_world::contention::CLAIM_TTL + Duration::from_secs(1));
    assert!(
        w.attention.is_empty(),
        "contention is a relation; it cannot outlive the claims"
    );
    assert!(w.claims.is_empty());
}

#[test]
fn a_landed_write_keeps_its_claim_and_a_failed_one_releases_it() {
    // PRD §11.3's early release is for the write that **did not happen**. A
    // write that did happen is not a reservation any more; it is a hazard, and
    // the thirty seconds is its duration. Deleting it on landing is what made
    // the operator's real collisions render as nothing: in a transcript the
    // result follows the call by under a second, so two workers 4.7 s apart
    // never held claims at the same instant.
    let mut w = world();
    let t0 = Instant::now();

    // ADR-0044: Channel A's `tool_result` is one of the two landing triggers.
    w.apply(&pre_edit("a", "src/auth.ts", "main", t0));
    assert_eq!(w.claims.len(), 1);
    w.apply(&tool_result(
        "a",
        None,
        ToolKind::Edit,
        "src/auth.ts",
        Outcome::Done,
        t0 + Duration::from_secs(1),
    ));
    let landed = w
        .claims
        .claims_on(&LogicalPath::new("src/auth.ts").unwrap());
    assert_eq!(landed.len(), 1, "the claim survives the write landing");
    assert!(landed[0].has_landed(), "and it says the bytes are on disk");

    // Which is what lets a sibling four seconds later collide with it.
    w.apply(&pre_edit(
        "b",
        "src/auth.ts",
        "main",
        t0 + Duration::from_secs(5),
    ));
    w.tick(t0 + Duration::from_secs(5));
    assert!(
        w.attention
            .iter()
            .any(|m| matches!(m.kind, AttentionKind::Contention(_))),
        "a write that landed is still a thing to write over"
    );

    // Channel C's `Modified` is the other landing trigger.
    w.apply(&pre_edit("a", "src/other.ts", "main", t0));
    let mut meta = EventMeta::now(Channel::Fs);
    meta.observed = t0 + Duration::from_secs(2);
    w.apply(&Event::new(
        meta,
        Payload::Fs(FsEvent::Modified {
            path: (
                WorktreeId::PRIMARY,
                LogicalPath::new("src/other.ts").unwrap(),
            ),
        }),
    ));
    let other = w
        .claims
        .claims_on(&LogicalPath::new("src/other.ts").unwrap());
    assert_eq!(other.len(), 1);
    assert!(other[0].has_landed());

    // A **failed** write wrote nothing, so its reservation goes at once — this
    // is the early release, doing the job it is actually for.
    w.apply(&pre_edit("a", "src/gone.ts", "main", t0));
    assert_eq!(
        w.claims
            .claims_on(&LogicalPath::new("src/gone.ts").unwrap())
            .len(),
        1
    );
    w.apply(&tool_result(
        "a",
        None,
        ToolKind::Edit,
        "src/gone.ts",
        Outcome::Failed,
        t0 + Duration::from_secs(1),
    ));
    assert!(
        w.claims
            .claims_on(&LogicalPath::new("src/gone.ts").unwrap())
            .is_empty(),
        "nothing was written, so there is nothing to write over"
    );
}

#[test]
fn a_permission_request_parks_the_thread_and_the_answer_frees_it() {
    let mut w = world();
    let t0 = Instant::now();
    w.apply(&hook(
        "a",
        EventKind::SessionStart,
        t0,
        HookPayload::default(),
    ));
    assert_eq!(
        w.thread(&thread("a")).unwrap().status,
        ThreadStatus::Working
    );

    w.apply(&hook(
        "a",
        EventKind::PermissionRequest,
        t0 + Duration::from_secs(1),
        HookPayload::default(),
    ));
    assert_eq!(
        w.thread(&thread("a")).unwrap().status,
        ThreadStatus::Waiting,
        "this is the state the product exists for"
    );
    match &w.attention[0].kind {
        AttentionKind::NeedsDecision {
            source, thread: t, ..
        } => {
            assert_eq!(*source, DecisionSource::PermissionRequest);
            assert_eq!(t, &thread("a"));
        }
        other => panic!("expected a pin, got {other:?}"),
    }

    // A second request from the same source is the same pin, not two.
    w.apply(&hook(
        "a",
        EventKind::PermissionRequest,
        t0 + Duration::from_secs(2),
        HookPayload::default(),
    ));
    assert_eq!(w.attention.len(), 1);

    // A `tool_decision` on Channel A means the human answered.
    let mut meta = EventMeta::now(Channel::Otel).with_session(SessionId::new("a"));
    meta.observed = t0 + Duration::from_secs(3);
    w.apply(&Event::new(
        meta,
        Payload::Otel(Box::new(OtelEvent::ToolDecision {
            tool: ToolKind::Edit,
            tool_use_id: Some(ToolUseId::new("toolu_1")),
            decision: Some("accept".to_owned()),
            source: Some("user".to_owned()),
        })),
    ));
    assert!(
        w.attention.is_empty(),
        "the pin persists only until resolved"
    );
    assert_eq!(
        w.thread(&thread("a")).unwrap().status,
        ThreadStatus::Working
    );
}

#[test]
fn a_pin_never_expires_while_anything_could_still_resolve_it() {
    let mut w = world();
    let t0 = Instant::now();
    w.apply(&hook(
        "a",
        EventKind::Notification,
        t0,
        HookPayload::default(),
    ));
    w.tick(t0 + MARK_HOLD_MAX);
    assert_eq!(
        w.attention.len(),
        1,
        "needs-decision persists until resolved, and hours of waiting is what          waiting on a human looks like"
    );
    assert_eq!(
        w.thread(&thread("a")).unwrap().status,
        ThreadStatus::Waiting
    );

    // The one end of it. No timer expires the pin — `AttentionKind::decays` is
    // still false for `NeedsDecision` — but past `MARK_HOLD_MAX` of total
    // silence `polis_ingest::live::LIVE_WINDOW` stopped following the session
    // seven retirement windows ago, so no channel can ever answer this
    // question. The thread is retired and the mark goes with it, which is the
    // difference between a queue and a rail that accumulates every session of
    // the day. Nothing here is a *timer on the pin*: it is the pin losing the
    // thread it was about.
    w.tick(t0 + MARK_HOLD_MAX + Duration::from_secs(1));
    assert!(w.thread(&thread("a")).is_none());
    assert!(w.attention.is_empty());
}

#[test]
fn done_is_split_by_verification_and_unverified_persists() {
    // PRD §11.2: without the split, `done` floods the display within the hour
    // and buries the state the product exists for.
    let mut w = world();
    let t0 = Instant::now();
    w.apply(&pre_edit("a", "src/auth.ts", "main", t0));
    w.apply(&hook(
        "a",
        EventKind::Stop,
        t0 + Duration::from_secs(1),
        HookPayload::default(),
    ));

    match &w.attention[0].kind {
        AttentionKind::Done { verified, .. } => assert!(
            !verified,
            "a thread that edited and never tested is really `needs review`"
        ),
        other => panic!("expected done, got {other:?}"),
    }
    w.tick(t0 + Duration::from_hours(1));
    assert_eq!(w.attention.len(), 1, "and it persists");
    assert_eq!(w.thread(&thread("a")).unwrap().status, ThreadStatus::Done);
}

#[test]
fn a_thread_with_nothing_to_review_finishes_verified_and_decays() {
    let mut w = world();
    let t0 = Instant::now();
    w.apply(&hook(
        "a",
        EventKind::SessionStart,
        t0,
        HookPayload::default(),
    ));
    w.apply(&tool_result(
        "a",
        None,
        ToolKind::Read,
        "src/auth.ts",
        Outcome::Done,
        t0,
    ));
    w.apply(&hook(
        "a",
        EventKind::Stop,
        t0 + Duration::from_secs(1),
        HookPayload::default(),
    ));
    match &w.attention[0].kind {
        AttentionKind::Done { verified, .. } => {
            assert!(
                verified,
                "a thread that wrote nothing has nothing to review"
            );
        }
        other => panic!("expected done, got {other:?}"),
    }
    w.tick(t0 + Duration::from_secs(120));
    assert!(
        w.attention.is_empty(),
        "done, verified decays to the base layer"
    );
}

#[test]
fn a_subagent_stopping_is_not_a_thread_finishing() {
    // "SubagentStop is noise; a main thread going idle is news."
    let mut w = world();
    let t0 = Instant::now();
    let payload = HookPayload {
        agent_id: Some(WorkerId::new("a123")),
        agent_type: Some(polis_events::AgentType::new("Explore")),
        ..HookPayload::default()
    };
    w.apply(&hook("a", EventKind::SubagentStop, t0, payload));
    assert!(
        w.attention.is_empty(),
        "no mark for a worker: {:?}",
        w.attention
    );
    let t = w.thread(&thread("a")).unwrap();
    assert_ne!(t.status, ThreadStatus::Done);
    assert_eq!(t.workers.len(), 1);
    assert!(!t.workers[0].running);
    assert_eq!(t.workers[0].kind.as_str(), "Explore");
}

#[test]
fn the_logs_channel_alone_cannot_attribute_a_subagent_and_says_so() {
    // ADR-0006: `agent_id` is on the `claude_code.tool` span and nowhere in the
    // logs. A session with the beta traces channel off attributes every call to
    // the main agent, and that is displayed, not guessed around.
    let mut w = world();
    let t0 = Instant::now();
    w.apply(&tool_result(
        "a",
        None,
        ToolKind::Edit,
        "src/auth.ts",
        Outcome::Done,
        t0,
    ));
    assert!(w.health.subagent_attribution_degraded);
    assert!(w.health.otel_tool_results_without_worker > 0);

    // One span is enough to prove the channel is on.
    let mut meta = EventMeta::now(Channel::Otel).with_session(SessionId::new("a"));
    meta.observed = t0 + Duration::from_secs(1);
    meta.worker = Some(WorkerId::new("a123"));
    w.apply(&Event::new(
        meta,
        Payload::Otel(Box::new(OtelEvent::ToolSpan {
            call: Box::new(ToolCall {
                tool: ToolKind::Read,
                tool_use_id: None,
                paths: vec![(WorktreeId::PRIMARY, LogicalPath::new("src/x.rs").unwrap())],
                outcome: Outcome::Done,
                duration_ms: None,
            }),
            span_id: Some("0123456789abcdef".to_owned()),
            parent_span_id: None,
        })),
    ));
    assert!(!w.health.subagent_attribution_degraded);
    assert_eq!(w.health.otel_tool_spans, 1);
    assert_eq!(w.thread(&thread("a")).unwrap().workers.len(), 1);
}

#[test]
fn a_degraded_channel_and_dropped_events_reach_the_status_bar() {
    let mut w = world();
    w.apply(&Event::control(ControlEvent::ChannelDegraded {
        channel: Channel::Otel,
        reason: "port 4317 already in use".to_owned(),
    }));
    w.apply(&Event::control(ControlEvent::EventsDropped {
        count: 17,
        channel: Channel::Hook,
    }));
    w.apply(&Event::control(ControlEvent::SequenceGap {
        session: SessionId::new("a"),
        expected: 5,
        got: 9,
    }));
    assert!(w.health.is_degraded());
    assert_eq!(
        w.health.degraded.get("otel").map(String::as_str),
        Some("port 4317 already in use")
    );
    assert_eq!(w.health.dropped.get("hook"), Some(&17));
    assert_eq!(w.health.sequence_gaps, 1);
    assert!(w.health.subagent_attribution_degraded);
}

#[test]
fn a_territory_emerges_only_once_the_observations_agree() {
    let mut w = world();
    let t0 = Instant::now();
    // One read: an unplaced marker, not a cloud.
    w.apply(&tool_result(
        "a",
        None,
        ToolKind::Read,
        "src/auth/token.rs",
        Outcome::Done,
        t0,
    ));
    assert!(w.thread(&thread("a")).unwrap().territory.claim.is_none());

    w.apply(&tool_result(
        "a",
        None,
        ToolKind::Edit,
        "src/auth/session.rs",
        Outcome::Done,
        t0 + Duration::from_secs(1),
    ));
    let t = w.thread(&thread("a")).unwrap();
    assert_eq!(
        t.territory.claim.as_ref().map(LogicalPath::as_str),
        Some("src/auth"),
        "two agreeing observations are enough; twelve are not required"
    );
    assert!(t.territory.has_converged());
    assert_eq!(t.territory.convergence().observations, 2);
}

#[test]
fn a_deleted_file_leaves_a_vacant_lot_rather_than_vanishing() {
    let mut w = world();
    let t0 = Instant::now();
    let path = LogicalPath::new("src/gone.rs").unwrap();
    let mut meta = EventMeta::now(Channel::Fs);
    meta.observed = t0;
    w.apply(&Event::new(
        meta.clone(),
        Payload::Fs(FsEvent::Created {
            path: (WorktreeId::PRIMARY, path.clone()),
        }),
    ));
    assert!(!w.file(&path).unwrap().deleted);

    let mut meta = EventMeta::now(Channel::Fs);
    meta.observed = t0 + Duration::from_secs(1);
    w.apply(&Event::new(
        meta,
        Payload::Fs(FsEvent::Removed {
            path: (WorktreeId::PRIMARY, path.clone()),
        }),
    ));
    assert!(w.file(&path).unwrap().deleted, "PRD §7.5: it goes to seed");
    assert!(w.files.contains_key(&path));
}

#[test]
fn a_rename_relocates_the_building_rather_than_demolishing_it() {
    let mut w = world();
    let t0 = Instant::now();
    let from = LogicalPath::new("src/old.rs").unwrap();
    let to = LogicalPath::new("src/new.rs").unwrap();
    w.apply(&tool_result(
        "a",
        None,
        ToolKind::Edit,
        "src/old.rs",
        Outcome::Done,
        t0,
    ));
    assert_eq!(w.file(&from).unwrap().writes, 1);

    let mut meta = EventMeta::now(Channel::Fs);
    meta.observed = t0 + Duration::from_secs(1);
    w.apply(&Event::new(
        meta,
        Payload::Fs(FsEvent::Renamed {
            from: (WorktreeId::PRIMARY, from.clone()),
            to: (WorktreeId::PRIMARY, to.clone()),
        }),
    ));
    assert!(w.file(&from).is_none());
    assert_eq!(
        w.file(&to).unwrap().writes,
        1,
        "the history moved with the file"
    );
}

/// One transcript record, as Channel D delivers it: the raw JSON verbatim.
fn transcript(
    session: &str,
    kind: polis_events::TranscriptRecordKind,
    at: Instant,
    offset: u64,
    record: serde_json::Value,
) -> Event {
    let mut meta = EventMeta::now(Channel::Transcript).with_session(SessionId::new(session));
    meta.observed = at;
    Event::new(
        meta,
        Payload::Transcript(Box::new(polis_events::TranscriptEvent {
            kind,
            source: polis_events::TranscriptSource::Main,
            byte_offset: offset,
            record,
        })),
    )
}

#[test]
fn an_edit_then_a_test_run_finishes_verified_with_an_exact_diff() {
    use polis_events::TranscriptRecordKind as K;
    let mut w = world();
    let t0 = Instant::now();
    let file = format!("{REPO}/src/auth.rs");

    // The edit.
    w.apply(&transcript(
        "a",
        K::Assistant,
        t0,
        0,
        serde_json::json!({
            "type": "assistant", "cwd": REPO, "gitBranch": "main",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_edit", "name": "Edit",
                 "input": {"file_path": file, "old_string": "a\nb", "new_string": "a\nb\nc"}}
            ]}
        }),
    ));
    // Its result, with the `structuredPatch` only a main transcript reliably has.
    w.apply(&transcript(
        "a",
        K::User,
        t0 + Duration::from_secs(1),
        1,
        serde_json::json!({
            "type": "user", "cwd": REPO, "gitBranch": "main",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_edit", "content": "ok"}
            ]},
            "toolUseResult": {
                "filePath": file,
                "structuredPatch": [
                    {"oldStart": 10, "oldLines": 2, "newStart": 10, "newLines": 3,
                     "lines": [" a", " b", "+c"]}
                ]
            }
        }),
    ));

    let path = LogicalPath::new("src/auth.rs").unwrap();
    let state = w.file(&path).expect("the edited file");
    assert_eq!(state.lines_added, 1);
    assert_eq!(state.lines_removed, 0);
    assert_eq!(
        state.diff_precision,
        polis_world::DiffPrecision::Exact,
        "a `structuredPatch` gives exact counts, and the file must say so"
    );
    assert!(!state.is_verified(), "nothing has tested it yet");

    // The test run. PRD §10.1's verify glyph has no tool of its own.
    w.apply(&transcript(
        "a",
        K::Assistant,
        t0 + Duration::from_secs(2),
        2,
        serde_json::json!({
            "type": "assistant", "cwd": REPO,
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_test", "name": "PowerShell",
                 "input": {"command": "cargo test --workspace"}}
            ]}
        }),
    ));
    let ops = &w.thread(&thread("a")).unwrap().ops;
    assert_eq!(
        ops.back().unwrap().glyph,
        polis_events::Glyph::ConcentricCircles,
        "a shell command that ran the suite is the verify glyph, not the run glyph"
    );

    w.apply(&transcript(
        "a",
        K::User,
        t0 + Duration::from_secs(3),
        3,
        serde_json::json!({
            "type": "user", "cwd": REPO,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_test", "content": "ok"}
            ]}
        }),
    ));
    assert!(
        w.file(&path).unwrap().is_verified(),
        "tests ran against the changed file after the change"
    );

    w.apply(&hook(
        "a",
        EventKind::Stop,
        t0 + Duration::from_secs(4),
        HookPayload::default(),
    ));
    match &w.attention[0].kind {
        AttentionKind::Done { verified, .. } => assert!(*verified),
        other => panic!("expected done, got {other:?}"),
    }
}

#[test]
fn a_subagent_edit_with_no_sidecar_degrades_and_marks_itself() {
    // ADR-0004: `toolUseResult` is absent on 65% of subagent tool results,
    // including 625 `Edit` calls. The line delta degrades to an approximation
    // and the line-range tier becomes unreachable — both stated, not hidden.
    use polis_events::TranscriptRecordKind as K;
    let mut w = world();
    let t0 = Instant::now();
    let file = format!("{REPO}/src/auth.rs");
    w.apply(&transcript(
        "a",
        K::Assistant,
        t0,
        0,
        serde_json::json!({
            "type": "assistant", "cwd": REPO,
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "Edit",
                 "input": {"file_path": file, "old_string": "a", "new_string": "a\nb\nc"}}
            ]}
        }),
    ));
    w.apply(&transcript(
        "a",
        K::User,
        t0 + Duration::from_secs(1),
        1,
        serde_json::json!({
            "type": "user", "cwd": REPO,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}
            ]}
        }),
    ));
    let state = w.file(&LogicalPath::new("src/auth.rs").unwrap()).unwrap();
    assert_eq!((state.lines_added, state.lines_removed), (3, 1));
    assert_eq!(
        state.diff_precision,
        polis_world::DiffPrecision::Approximate
    );
    // The edit lacked a line range; nothing collided with it. Those are two
    // different counters and this test is about the first.
    assert!(w.health.edits_without_line_ranges > 0);
    assert_eq!(
        w.health.contention_without_line_ranges, 0,
        "a lone degraded edit is not a contention hit"
    );
}

#[test]
fn an_unmodelled_payload_is_drift_and_never_fatal() {
    // PRD §4.1: unknown event types are logged at debug and dropped.
    let mut w = world();
    let mut meta = EventMeta::now(Channel::Otel).with_session(SessionId::new("a"));
    meta.observed = Instant::now();
    w.apply(&Event::new(
        meta,
        Payload::Otel(Box::new(OtelEvent::Unknown {
            name: "claude_code.something_new".to_owned(),
        })),
    ));
    assert_eq!(w.health.drift, 1);
    assert_eq!(w.health.events_applied, 1);
    assert_eq!(w.threads.len(), 1, "the thread still exists");
}
