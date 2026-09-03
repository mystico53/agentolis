//! PRD §11.2 state (a), reconstructed from Channel D.
//!
//! # Why this file exists
//!
//! §11.2 sources "needs decision" from hooks — `PermissionRequest`,
//! `Elicitation`, `TeammateIdle` — and calls it *"the primary state; it is what
//! the product is for"*. A replayed transcript is **Channel D**, so none of
//! those three can arrive, and the consequence was measured rather than
//! guessed: across all three M2 recordings the attention band was 0.000% of map
//! area in every one of 1 440 frames. The recorded artefact could not show the
//! thing the product exists for.
//!
//! Three rules close as much of that gap as a transcript allows. Two are
//! **prospective** — the mark's onset and duration are the real ones — and one
//! is retrospective and says so. This file pins all three, plus the resolution
//! rules, because the failure mode they replace was silent.
//!
//! What a transcript still cannot produce is in `docs/replay/README.md`.

use std::time::Instant;

use polis_events::{
    Channel, Event, EventMeta, Payload, SessionId, ThreadId, TranscriptEvent, TranscriptRecordKind,
    TranscriptSource, WorkerId, WorktreeId,
};
use polis_layout::CityLayout;
use polis_world::attention::{AttentionKind, DecisionSource};
use polis_world::{ThreadStatus, World};

const REPO: &str = "C:/repo";
const SESSION: &str = "s1";

fn world() -> World {
    let mut w = World::for_replay(CityLayout::default());
    w.mapper_mut()
        .add_worktree(WorktreeId::PRIMARY, std::path::Path::new(REPO))
        .expect("primary root");
    w
}

fn thread() -> ThreadId {
    ThreadId::of_session(SessionId::new(SESSION))
}

/// One transcript record, as `polis-ingest` hands it over.
fn record(
    kind: TranscriptRecordKind,
    worker: Option<&str>,
    at: Instant,
    mut body: serde_json::Value,
) -> Event {
    body["cwd"] = serde_json::json!(REPO);
    body["sessionId"] = serde_json::json!(SESSION);
    if let Some(w) = worker {
        body["agentId"] = serde_json::json!(w);
    }
    let mut meta = EventMeta::now(Channel::Transcript).with_session(SessionId::new(SESSION));
    meta.observed = at;
    meta.worker = worker.map(WorkerId::new);
    let source = worker.map_or(TranscriptSource::Main, |w| TranscriptSource::Subagent {
        agent: WorkerId::new(w),
        workflow_run: None,
    });
    Event::new(
        meta,
        Payload::Transcript(Box::new(TranscriptEvent {
            kind,
            source,
            byte_offset: 0,
            record: body,
        })),
    )
}

/// An assistant record making one tool call.
fn calls(tool: &str, id: &str, worker: Option<&str>, at: Instant) -> Event {
    record(
        TranscriptRecordKind::Assistant,
        worker,
        at,
        serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "stop_reason": "tool_use",
                "content": [{
                    "type": "tool_use", "id": id, "name": tool,
                    "input": { "file_path": format!("{REPO}/src/a.rs") }
                }]
            }
        }),
    )
}

/// An assistant record that ends the turn with prose and no tool call.
fn ends_turn(worker: Option<&str>, at: Instant) -> Event {
    record(
        TranscriptRecordKind::Assistant,
        worker,
        at,
        serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "stop_reason": "end_turn",
                "content": [{ "type": "text", "text": "Done. Anything else?" }]
            }
        }),
    )
}

/// A `tool_result` answering one call.
fn result(id: &str, worker: Option<&str>, at: Instant, extra: &serde_json::Value) -> Event {
    let mut body = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{ "type": "tool_result", "tool_use_id": id, "content": "ok" }]
        }
    });
    if let Some(map) = extra.as_object() {
        for (k, v) in map {
            body[k] = v.clone();
        }
    }
    record(TranscriptRecordKind::User, worker, at, body)
}

/// A human typing.
fn human(at: Instant, meta: bool) -> Event {
    let mut body = serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": "carry on" }
    });
    if meta {
        body["isMeta"] = serde_json::json!(true);
    }
    record(TranscriptRecordKind::User, None, at, body)
}

fn decisions(world: &World) -> Vec<DecisionSource> {
    world
        .attention
        .iter()
        .filter_map(|m| match &m.kind {
            AttentionKind::NeedsDecision { source, .. } => Some(*source),
            _ => None,
        })
        .collect()
}

#[test]
fn a_tool_that_asks_the_operator_raises_the_primary_state_when_it_is_called() {
    // `AskUserQuestion` and `ExitPlanMode` block on a human by construction, so
    // the mark can be raised *at the call* — no lookahead, the same instant a
    // `PermissionRequest` hook would have fired. Measured in the operator's own
    // corpus: seven of these in session 29c2fc6f, waits from 42 s to 2 h.
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&calls("AskUserQuestion", "t1", None, t0));
    assert_eq!(decisions(&w), vec![DecisionSource::AskUser]);
    assert_eq!(w.threads[&thread()].status, ThreadStatus::Waiting);
    assert!(DecisionSource::AskUser.is_reconstructed());
    assert!(
        DecisionSource::AskUser.onset_is_exact(),
        "the question is the tool call, so the onset is the real one"
    );
}

#[test]
fn an_ordinary_tool_call_raises_nothing() {
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&calls("Read", "t1", None, t0));
    assert!(decisions(&w).is_empty());
    assert_eq!(w.threads[&thread()].status, ThreadStatus::Working);
}

#[test]
fn a_main_agent_ending_its_turn_is_waiting_on_a_human() {
    // The transcript's form of §11.2's `idle_prompt` / `agent_needs_input`, and
    // by far the most common: 62 / 38 / 6 of them in the three recorded
    // sessions, median waits of 5, 13 and 27 minutes. This is the rule that
    // makes a replay able to show the primary state at all.
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&ends_turn(None, t0));
    assert_eq!(decisions(&w), vec![DecisionSource::TurnEnded]);
    assert_eq!(w.threads[&thread()].status, ThreadStatus::Waiting);
}

#[test]
fn a_subagent_ending_its_turn_is_not_news() {
    // The same argument PRD §11.2 makes for `done`: `SubagentStop` is noise; a
    // *main* thread going idle is news. A 69-worker fleet would otherwise raise
    // a pin every time any one of them finished a sentence.
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&calls("Agent", "t0", None, t0));
    w.apply(&ends_turn(Some("a1"), t0));
    assert!(decisions(&w).is_empty());
}

#[test]
fn a_streamed_fragment_with_no_stop_reason_does_not_flicker_a_pin() {
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&record(
        TranscriptRecordKind::Assistant,
        None,
        t0,
        serde_json::json!({
            "type": "assistant",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "one moment" }] }
        }),
    ));
    assert!(decisions(&w).is_empty(), "no stop_reason is not a turn end");
}

#[test]
fn the_human_answering_clears_the_wait() {
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&ends_turn(None, t0));
    assert_eq!(decisions(&w).len(), 1);
    w.apply(&human(t0, false));
    assert!(decisions(&w).is_empty());
    assert_eq!(w.threads[&thread()].status, ThreadStatus::Working);
}

#[test]
fn an_injected_record_is_not_the_human_answering() {
    // 273 `isMeta` records in the corpus and 69 in one recorded session. They
    // are the harness talking to itself, not the operator coming back.
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&ends_turn(None, t0));
    w.apply(&human(t0, true));
    assert_eq!(decisions(&w), vec![DecisionSource::TurnEnded]);
}

#[test]
fn the_main_agent_making_a_call_clears_the_wait_but_a_worker_does_not() {
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&calls("Agent", "t0", None, t0));
    w.apply(&ends_turn(None, t0));
    assert_eq!(decisions(&w).len(), 1);

    // A subagent churning away says nothing about whether the operator replied.
    w.apply(&calls("Read", "t1", Some("a1"), t0));
    assert_eq!(decisions(&w).len(), 1, "a worker's call is not an answer");

    w.apply(&calls("Read", "t2", None, t0));
    assert!(decisions(&w).is_empty(), "the main thread made progress");
}

#[test]
fn a_denied_tool_call_proves_the_operator_was_asked() {
    // `toolDenialKind` ∈ user-rejected | permission-rule | automode-blocked |
    // automode-unavailable. 20 of them in 29c2fc6f, 8 in 6f51089f. It is the
    // *answer*, so the mark is late by however long the operator took — which
    // `onset_is_exact` says out loud rather than pretending otherwise.
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&calls("Edit", "t1", None, t0));
    w.apply(&result(
        "t1",
        None,
        t0,
        &serde_json::json!({ "toolDenialKind": "user-rejected" }),
    ));
    assert_eq!(decisions(&w), vec![DecisionSource::Rejected]);
    assert!(!DecisionSource::Rejected.onset_is_exact());
    assert_eq!(w.threads[&thread()].status, ThreadStatus::Waiting);
}

#[test]
fn an_interrupt_is_the_same_evidence() {
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&calls("Bash", "t1", None, t0));
    w.apply(&record(
        TranscriptRecordKind::User,
        None,
        t0,
        serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": "[Request interrupted by user]" }]
            }
        }),
    ));
    assert_eq!(decisions(&w), vec![DecisionSource::Rejected]);
}

#[test]
fn an_ordinary_result_neither_raises_nor_clears() {
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&ends_turn(None, t0));
    w.apply(&calls("Read", "t1", None, t0));
    assert!(decisions(&w).is_empty());
    w.apply(&result("t1", None, t0, &serde_json::json!({})));
    assert!(decisions(&w).is_empty());
    assert_eq!(w.threads[&thread()].status, ThreadStatus::Working);
}

#[test]
fn every_channel_d_source_is_labelled_as_reconstructed() {
    // PRD §17: anything that drives an alert must come from an authoritative
    // channel, and a recording is not one. The distinction has to survive into
    // the UI, so it lives on the type.
    for s in [
        DecisionSource::AskUser,
        DecisionSource::TurnEnded,
        DecisionSource::Rejected,
    ] {
        assert!(s.is_reconstructed(), "{s:?}");
        assert!(!s.label().is_empty());
    }
    for s in [
        DecisionSource::PermissionRequest,
        DecisionSource::Elicitation,
        DecisionSource::TeammateIdle,
        DecisionSource::Notification,
    ] {
        assert!(!s.is_reconstructed(), "{s:?}");
        assert!(s.onset_is_exact());
    }
}

#[test]
fn one_thread_waiting_is_one_pin_however_many_rules_saw_it() {
    // A live session tails Channel D as well as Channel B (PRD §4.4), so a
    // reconstruction and a hook can see the same wait. §11.2 draws **a**
    // standing pin, not one per reason.
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&calls("Edit", "t1", None, t0));
    w.apply(&result(
        "t1",
        None,
        t0,
        &serde_json::json!({ "toolDenialKind": "user-rejected" }),
    ));
    w.apply(&ends_turn(None, t0));
    assert_eq!(
        decisions(&w),
        vec![DecisionSource::TurnEnded],
        "a reconstruction replaces a reconstruction"
    );
}

#[test]
fn a_hook_supersedes_the_reconstruction_and_is_not_displaced_by_it() {
    use polis_events::{EventKind, HookEvent, HookPayload};
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&ends_turn(None, t0));
    assert_eq!(decisions(&w), vec![DecisionSource::TurnEnded]);

    let mut payload = HookPayload {
        session_id: Some(SessionId::new(SESSION)),
        hook_event_name: Some("PermissionRequest".to_owned()),
        cwd: Some(REPO.to_owned()),
        ..HookPayload::default()
    };
    payload.tool_name = Some("Edit".to_owned());
    let mut meta = EventMeta::now(Channel::Hook).with_session(SessionId::new(SESSION));
    meta.observed = t0;
    let hook = Event::new(
        meta,
        Payload::Hook(Box::new(HookEvent {
            kind: EventKind::PermissionRequest,
            truncated: false,
            payload,
        })),
    );
    w.apply(&hook);
    assert_eq!(
        decisions(&w),
        vec![DecisionSource::PermissionRequest],
        "the authoritative channel replaces the guess"
    );

    // And the guess does not come back and sit beside it.
    w.apply(&ends_turn(None, t0));
    assert_eq!(decisions(&w), vec![DecisionSource::PermissionRequest]);
}

#[test]
fn only_the_two_tools_that_block_on_a_human_count_as_asking() {
    use polis_events::ToolKind;
    use polis_world::attention::asks_the_operator;
    assert!(asks_the_operator(&ToolKind::parse("AskUserQuestion")));
    assert!(asks_the_operator(&ToolKind::parse("ExitPlanMode")));
    assert!(!asks_the_operator(&ToolKind::parse("Read")));
    assert!(!asks_the_operator(&ToolKind::parse("Bash")));
    assert!(!asks_the_operator(&ToolKind::parse("mcp__x__ask")));
}
