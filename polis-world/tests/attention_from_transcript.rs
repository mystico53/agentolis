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
fn a_main_agent_ending_its_turn_is_ready_and_asks_for_nothing() {
    // This used to raise `DecisionSource::TurnEnded` and paint the loudest state
    // in the product on the most common thing an agent does. PRD §11.2 lists the
    // sources of *needs decision* and a turn ending is not among them; §11.2(b)
    // files it under `done` instead.
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&ends_turn(None, t0));
    assert!(
        decisions(&w).is_empty(),
        "a finished turn owes the operator no answer"
    );
    assert_eq!(w.threads[&thread()].status, ThreadStatus::Ready);
    assert!(!w.threads[&thread()].status.blocks_operator());
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
    // The wait has to be a real one. This used to lean on `ends_turn`, which
    // raised a mark for every finished turn — the very conflation that made
    // `WAITING` meaningless.
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&calls("AskUserQuestion", "q1", None, t0));
    assert_eq!(decisions(&w).len(), 1);
    assert_eq!(w.threads[&thread()].status, ThreadStatus::Waiting);
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
    assert_eq!(
        w.threads[&thread()].status,
        ThreadStatus::Ready,
        "an injected record does not resume the thread"
    );
}

#[test]
fn the_main_agent_making_a_call_clears_the_wait_but_a_worker_does_not() {
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&calls("Agent", "t0", None, t0));
    w.apply(&calls("AskUserQuestion", "q1", None, t0));
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
    // A denial is the ANSWER, not the question. It used to raise a pending
    // decision, which rendered on the operator's live map as "WAITING ON YOU ·
    // you said no" — telling them they owed a reply to something they had
    // already declined. Their report was exact: *"this says waiting for me, but
    // the chat says done"*. PRD §11.2a reserves that state for a thread
    // genuinely blocked on a human, so the evidence now RESOLVES.
    assert!(
        decisions(&w).is_empty(),
        "a rejection is an answer and must not raise a pending decision"
    );
    assert_ne!(
        w.threads[&thread()].status,
        ThreadStatus::Waiting,
        "the operator already answered, so the thread is not waiting on them"
    );
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
    // Same reasoning as a denial: an interrupt is the operator acting, not the
    // operator being asked.
    assert!(
        decisions(&w).is_empty(),
        "an interrupt is the operator's own answer, not a question for them"
    );
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
    for s in [DecisionSource::AskUser, DecisionSource::Rejected] {
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
    assert!(
        decisions(&w).is_empty(),
        "a rejection is an answer and a finished turn is not a question"
    );
}

#[test]
fn a_hook_supersedes_the_reconstruction_and_is_not_displaced_by_it() {
    use polis_events::{EventKind, HookEvent, HookPayload};
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&calls("AskUserQuestion", "q1", None, t0));
    assert_eq!(decisions(&w), vec![DecisionSource::AskUser]);

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

// ---------------------------------------------------------------------------
// The states a terminal is actually in
//
// Every fixture below is cut from a sequence that really happened on the
// operator's machine, named in each test. The three complaints these close were
// all one symptom — a thread that had stopped for four different reasons showed
// one word — so they are asserted as four different words.
// ---------------------------------------------------------------------------

/// An interrupt at the prompt.
fn interrupt(text: &str, at: Instant) -> Event {
    record(
        TranscriptRecordKind::User,
        None,
        at,
        serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": text }] }
        }),
    )
}

/// A `tool_result` that launched a background job.
fn launches_background(id: &str, task: &str, at: Instant) -> Event {
    result(
        id,
        None,
        at,
        &serde_json::json!({
            "toolUseResult": { "backgroundTaskId": task, "command": "cargo test --workspace" }
        }),
    )
}

/// The `<task-notification>` that closes one.
fn task_notification(task: &str, at: Instant) -> Event {
    record(
        TranscriptRecordKind::User,
        None,
        at,
        serde_json::json!({
            "type": "user",
            "origin": { "kind": "task-notification" },
            "message": {
                "role": "user",
                "content": format!("<task-notification><task-id>{task}</task-id><status>completed</status></task-notification>")
            }
        }),
    )
}

/// Both wordings, because `for tool use` is 41 of this machine's 86 interrupts
/// and 15 of those are the last record in their file. Reading it as a mere tool
/// rejection left half of all interrupts decaying into `Idle`.
#[test]
fn either_wording_of_an_interrupt_says_interrupted() {
    for text in [
        "[Request interrupted by user]",
        "[Request interrupted by user for tool use]",
    ] {
        let t0 = Instant::now();
        let mut w = world();
        w.apply(&calls("Bash", "t1", None, t0));
        w.apply(&interrupt(text, t0));
        assert_eq!(
            w.threads[&thread()].status,
            ThreadStatus::Interrupted,
            "{text}"
        );
        assert!(w.threads[&thread()].status.blocks_operator(), "{text}");
        assert!(
            decisions(&w).is_empty(),
            "an interrupt asks nothing: {text}"
        );
    }
}

/// Cut from `72e11b78` idx 313 → 315, where a notification lands 429 ms after
/// the interrupt and the thread carries straight on. A sticky `Interrupted`
/// would hold a blocking pin over a visibly working thread.
#[test]
fn work_after_an_interrupt_clears_it() {
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&interrupt("[Request interrupted by user]", t0));
    assert_eq!(w.threads[&thread()].status, ThreadStatus::Interrupted);
    w.apply(&calls("Read", "t9", None, t0));
    assert_eq!(w.threads[&thread()].status, ThreadStatus::Working);
    assert!(w.threads[&thread()].interrupted_at.is_none());
}

/// The operator's complaint, in one test: *"its waiting on the clean run before
/// committing, not for the user"*.
#[test]
fn a_turn_that_ends_over_a_background_job_is_parked_not_waiting() {
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&calls("Bash", "t1", None, t0));
    w.apply(&launches_background("t1", "bg-1", t0));
    w.apply(&ends_turn(None, t0));

    let t = &w.threads[&thread()];
    assert_eq!(t.status, ThreadStatus::Parked);
    assert!(
        !t.status.blocks_operator(),
        "a parked thread resumes itself; it is not the operator's move"
    );
    assert!(decisions(&w).is_empty());
    assert_eq!(t.background.len(), 1);
    assert_eq!(
        t.background[0].label.as_deref(),
        Some("cargo test --workspace")
    );
}

/// *"then it starts again automatically and the 'waiting' disappears"* — the
/// notification is the machine waking the thread, not the operator answering.
#[test]
fn a_task_notification_resumes_a_parked_thread_without_answering_anything() {
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&calls("Bash", "t1", None, t0));
    w.apply(&launches_background("t1", "bg-1", t0));
    w.apply(&ends_turn(None, t0));
    assert_eq!(w.threads[&thread()].status, ThreadStatus::Parked);

    w.apply(&task_notification("bg-1", t0));
    let t = &w.threads[&thread()];
    assert!(t.background.is_empty(), "the notification closes its job");
    assert_eq!(t.status, ThreadStatus::Working);
}

/// A machine wake must not take down a pin nobody answered. 218 such records in
/// this machine's corpus were being read as the operator replying.
#[test]
fn a_machine_wake_does_not_answer_a_pending_question() {
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&calls("AskUserQuestion", "q1", None, t0));
    assert_eq!(decisions(&w), vec![DecisionSource::AskUser]);
    w.apply(&task_notification("bg-1", t0));
    assert_eq!(
        decisions(&w),
        vec![DecisionSource::AskUser],
        "a background job reporting in is not the operator answering"
    );
    assert_eq!(w.threads[&thread()].status, ThreadStatus::Waiting);
}

/// A question outranks a background job: the operator is the bottleneck even
/// when the machine is also busy.
#[test]
fn a_real_question_outranks_a_background_job() {
    let t0 = Instant::now();
    let mut w = world();
    w.apply(&calls("Bash", "t1", None, t0));
    w.apply(&launches_background("t1", "bg-1", t0));
    w.apply(&calls("AskUserQuestion", "q1", None, t0));
    assert_eq!(w.threads[&thread()].status, ThreadStatus::Waiting);
    assert!(w.threads[&thread()].status.blocks_operator());
}

/// A backgrounded `cargo test` has *started*, not passed. Marking the thread
/// verified at launch would clear PRD §11.2's "done, unverified" the instant the
/// command was fired — the opposite of what the operator meant by *"waiting on
/// the clean run before committing"*.
#[test]
fn launching_a_background_test_does_not_count_as_verifying() {
    let t0 = Instant::now();

    // The same command in the foreground *is* a verification, so the two halves
    // of this test differ only in whether the result was backgrounded.
    let mut fg = world();
    fg.apply(&runs_tests("t1", t0));
    fg.apply(&result("t1", None, t0, &serde_json::json!({})));
    assert!(
        fg.threads[&thread()].last_verified.is_some(),
        "a foreground test run that passed verifies the thread"
    );

    let mut bg = world();
    bg.apply(&runs_tests("t1", t0));
    bg.apply(&launches_background("t1", "bg-1", t0));
    assert!(
        bg.threads[&thread()].last_verified.is_none(),
        "a test that was launched is not a test that passed"
    );
    assert_eq!(bg.threads[&thread()].background.len(), 1);
}

/// A `Bash` call whose command line is a test run.
fn runs_tests(id: &str, at: Instant) -> Event {
    record(
        TranscriptRecordKind::Assistant,
        None,
        at,
        serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "stop_reason": "tool_use",
                "content": [{
                    "type": "tool_use", "id": id, "name": "Bash",
                    "input": { "command": "cargo test --workspace" }
                }]
            }
        }),
    )
}
