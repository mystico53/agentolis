//! PRD §11.2's attention layer, fired by **Channel B**, end to end and on
//! pixels.
//!
//! # Why this file exists
//!
//! Two of the three states cannot be produced by anything M2 could run:
//!
//! * *done* needs `Stop` / `SessionEnd`, which are **hooks**. A transcript has
//!   no record of a session ending, so a replay cannot raise it. Measured across
//!   six real sessions and 1 440 sample points: **zero** `done` marks, ever
//!   (`polis-world/tests/attention_states.rs`);
//! * *needs decision* has four authoritative sources —`PermissionRequest`,
//!   `Elicitation`, `TeammateIdle`, `Notification` — and all four are hooks. The
//!   replay reconstructs three weaker ones from Channel D and says so
//!   ([`polis_world::attention::DecisionSource::is_reconstructed`]).
//!
//! So the attention layer has been a set of shapes nobody had seen fire. This
//! fires them, through the real wire path and nothing else:
//!
//! ```text
//! UDP datagram  ->  hook_listener::decode_datagram  ->  to_event
//!               ->  World::apply  ->  FrameRenderer  ->  pixels
//! ```
//!
//! # The payloads are real
//!
//! [`SESSION`], [`CWD`] and the body of every frame below were **captured from a
//! live Claude Code session** on 2026-09-03: a scratch project with a
//! project-local `.claude/settings.json` pointing every hook at the release
//! `polis-hook` binary, `POLIS_HOOK_ENDPOINT` aimed at a capture socket, and one
//! prompt. Six datagrams arrived — `SessionStart`, `UserPromptSubmit`,
//! `PreToolUse`, `PostToolUse`, `Stop`, `SessionEnd` — and their field sets are
//! reproduced here verbatim, with the session id and the operator's paths
//! replaced. `UserPromptSubmit` and `PostToolUse` arrived with **tag 0**, which
//! is correct and deliberate: neither is in [`EventKind`], the daemon re-derives
//! the kind from `hook_event_name`, and the firehose belongs on Channel A.
//!
//! The two payloads a trivial prompt could not produce — `PermissionRequest` and
//! `Notification` — are built to `docs/verified/hooks-schema.md` §5's recorded
//! shapes instead.

#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::time::{Duration, Instant};

use polis_events::{
    Channel, Event, EventKind, EventMeta, LogicalPath, Payload, SessionId, ThreadId,
    TranscriptEvent, TranscriptRecordKind, TranscriptSource,
};
use polis_ingest::hook_listener::{decode_datagram, to_event};
use polis_layout::city::{self, City};
use polis_render::frame::{FrameOptions, FrameRenderer};
use polis_render::live::MarkKind;
use polis_render::plan::ATTENTION_BAND;
use polis_render::raster::Canvas;
use polis_repo::{synthetic, RepoTree};
use polis_world::attention::{AttentionKind, DecisionSource};
use polis_world::{snapshot, World};

/// The session id from the captured run, replaced with a stable one.
const SESSION: &str = "40fe953e-32c1-4e9b-8f19-bb9368680fc7";

/// The working directory every captured payload carried.
const CWD: &str = "C:/repo";

/// One datagram in `polis-hook`'s wire format (PRD §4.2):
/// `u32 LE tag | u32 LE len | payload`.
///
/// Built here rather than imported so the test exercises the **decoder** and not
/// a shared encoder: a change to the framing that both sides make together is
/// exactly the change this would otherwise fail to catch.
fn datagram(kind: EventKind, body: &str) -> Vec<u8> {
    let payload = body.as_bytes();
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&(kind as u32).to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// The common envelope every captured payload carried.
fn envelope(name: &str) -> String {
    format!(
        r#""session_id":"{SESSION}","transcript_path":"{CWD}/.jsonl","cwd":"{CWD}","prompt_id":"5a337d73-4994-4a47-8f6c-7c37f7e26a34","permission_mode":"acceptEdits","hook_event_name":"{name}""#
    )
}

fn feed(world: &mut World, kind: EventKind, body: &str, at: Instant) {
    let frame = datagram(kind, body);
    let mut event = to_event(decode_datagram(&frame).expect("a well-formed datagram"));
    event.meta.observed = at;
    world.apply(&event);
    world.tick(at);
}

fn thread() -> ThreadId {
    ThreadId::of_session(SessionId::new(SESSION))
}

fn city() -> City {
    city::generate_city(&synthetic::repository(240, 0x51))
}

fn world_for(city: &City) -> World {
    let mut world = World::new(RepoTree::default(), city.layout.clone());
    world
        .mapper_mut()
        .add_worktree(polis_events::WorktreeId::PRIMARY, std::path::Path::new(CWD))
        .expect("primary root");
    world
}

/// Ink in the band PRD §10.3 reserves for layer 5.
fn attention_px(canvas: &Canvas) -> usize {
    canvas
        .pixels
        .as_chunks::<3>()
        .0
        .iter()
        .filter(|p| p.iter().copied().max().unwrap_or(0) >= ATTENTION_BAND.0)
        .count()
}

fn render(city: &City, world: &World) -> Canvas {
    let mut r = FrameRenderer::new(
        city,
        FrameOptions {
            pixels: 480,
            supersample: 1,
            caption: false,
            rail: false,
            ..FrameOptions::default()
        },
    );
    let (_, reader) = snapshot::from_world(world);
    let snap = reader.load();
    r.render_owned(&snap, Duration::from_millis(40))
}

/// **The primary state, fired by the channel PRD §11.2 sources it from.**
///
/// > **(a) Needs decision** — persistent, amber […] Sources: `PermissionRequest`,
/// > `Elicitation`, `TeammateIdle`. […] This is the primary state; it is what
/// > the product is for.
#[test]
fn a_permission_request_hook_raises_the_primary_state_and_draws_a_pin() {
    let c = city();
    let mut world = world_for(&c);
    let t0 = Instant::now();

    // The session exists first, exactly as the live run delivered it.
    feed(
        &mut world,
        EventKind::SessionStart,
        &format!(r#"{{{},"source":"startup"}}"#, envelope("SessionStart")),
        t0,
    );
    // …and it has done a little work, so it has somewhere to be drawn.
    let path = c
        .layout
        .buildings
        .keys()
        .next()
        .expect("a building")
        .clone();
    feed(
        &mut world,
        EventKind::PreToolUse,
        &format!(
            r#"{{{},"tool_name":"Edit","tool_input":{{"file_path":"{CWD}/{path}"}},"tool_use_id":"toolu_01"}}"#,
            envelope("PreToolUse")
        ),
        t0 + Duration::from_secs(1),
    );
    let before = render(&c, &world);
    assert_eq!(
        attention_px(&before),
        0,
        "the attention band is not empty before anything asked for a decision"
    );

    feed(
        &mut world,
        EventKind::PermissionRequest,
        &format!(
            r#"{{{},"tool_name":"Bash","tool_input":{{"command":"rm -rf build"}}}}"#,
            envelope("PermissionRequest")
        ),
        t0 + Duration::from_secs(2),
    );

    let marks: Vec<&AttentionKind> = world.attention.iter().map(|m| &m.kind).collect();
    assert_eq!(marks.len(), 1, "expected one pin, got {marks:?}");
    match marks[0] {
        AttentionKind::NeedsDecision {
            thread: t, source, ..
        } => {
            assert_eq!(t, &thread());
            assert_eq!(*source, DecisionSource::PermissionRequest);
            assert!(
                !source.is_reconstructed(),
                "a hook is authoritative and must not be labelled a reconstruction"
            );
        }
        other => panic!("expected needs-decision, got {other:?}"),
    }
    assert_eq!(
        world.threads[&thread()].status,
        polis_world::ThreadStatus::Waiting,
        "the thread must be parked on the pin"
    );

    let after = render(&c, &world);
    let px = attention_px(&after);
    eprintln!("  PermissionRequest -> {px} px in the attention band");
    assert!(
        px > 0,
        "the pin fired in the world and drew nothing on the map"
    );
}

/// The other three authoritative sources, each raising the same state.
#[test]
fn every_authoritative_source_raises_the_pin_and_the_answer_clears_it() {
    let c = city();
    for (kind, name, body, want) in [
        (
            EventKind::Elicitation,
            "Elicitation",
            r#","message":"Please provide your credentials""#,
            DecisionSource::Elicitation,
        ),
        (
            EventKind::TeammateIdle,
            "TeammateIdle",
            "",
            DecisionSource::TeammateIdle,
        ),
        (
            EventKind::Notification,
            "Notification",
            r#","message":"Claude needs your permission","notification_type":"permission_prompt""#,
            DecisionSource::Notification,
        ),
    ] {
        let mut world = world_for(&c);
        let t0 = Instant::now();
        feed(
            &mut world,
            kind,
            &format!("{{{}{body}}}", envelope(name)),
            t0,
        );
        let sources: Vec<DecisionSource> = world
            .attention
            .iter()
            .filter_map(|m| match &m.kind {
                AttentionKind::NeedsDecision { source, .. } => Some(*source),
                _ => None,
            })
            .collect();
        assert_eq!(
            sources,
            vec![want],
            "{name} did not raise the primary state"
        );
    }

    // …and `ElicitationResult` is the human answering, which ends the wait.
    let mut world = world_for(&c);
    let t0 = Instant::now();
    feed(
        &mut world,
        EventKind::Elicitation,
        &format!("{{{}}}", envelope("Elicitation")),
        t0,
    );
    assert_eq!(world.attention.len(), 1);
    feed(
        &mut world,
        EventKind::ElicitationResult,
        &format!("{{{}}}", envelope("ElicitationResult")),
        t0 + Duration::from_secs(30),
    );
    assert!(
        world.attention.is_empty(),
        "the answer did not clear the pin: {:?}",
        world.attention
    );
}

/// **(b) Done**, which no replay can produce.
///
/// > *Done, verified* (tests ran against the changed files after the change):
/// > full weight 20s, then decays. *Done, unverified*: **persists.**
///
/// The captured `Stop` payload, verbatim in its field set, driven twice: once
/// over a thread that wrote a file and never tested it, and once over one that
/// ran its tests afterwards.
#[test]
fn a_real_stop_hook_raises_done_and_the_verification_split_is_real() {
    let c = city();
    let path = c
        .layout
        .buildings
        .keys()
        .next()
        .expect("a building")
        .clone();
    let stop = format!(
        r#"{{{},"stop_hook_active":false,"last_assistant_message":"ok","background_tasks":[],"session_crons":[]}}"#,
        envelope("Stop")
    );

    let mut unverified = world_for(&c);
    let t0 = Instant::now();
    write_a_file(&mut unverified, &path, t0);
    feed(
        &mut unverified,
        EventKind::Stop,
        &stop,
        t0 + Duration::from_secs(5),
    );
    let kinds: Vec<_> = unverified.attention.iter().map(|m| &m.kind).collect();
    assert!(
        matches!(
            kinds.as_slice(),
            [AttentionKind::Done {
                verified: false,
                ..
            }]
        ),
        "a session that wrote and never tested must be `needs review`: {kinds:?}"
    );
    assert!(
        !kinds[0].decays(),
        "PRD §11.2b: done-unverified persists — it is really `needs review`"
    );

    let mut verified = world_for(&c);
    write_a_file(&mut verified, &path, t0);
    ran_the_tests(&mut verified, t0 + Duration::from_secs(3));
    feed(
        &mut verified,
        EventKind::Stop,
        &stop,
        t0 + Duration::from_secs(5),
    );
    let kinds: Vec<_> = verified.attention.iter().map(|m| &m.kind).collect();
    assert!(
        matches!(
            kinds.as_slice(),
            [AttentionKind::Done { verified: true, .. }]
        ),
        "tests ran after the change and it is still unverified: {kinds:?}"
    );
    assert!(kinds[0].decays(), "done-verified decays to the base layer");

    // PRD §17 q2, as a rank rather than a fourth state: needs-review outranks
    // finished. Measured at 52.7 % of thread-samples on the real corpus.
    assert!(
        unverified.attention[0].kind.rank() < verified.attention[0].kind.rank(),
        "needs review must sort above finished"
    );

    // And the two draw different shapes, which is what stops the split from
    // being colour-only.
    let a = render(&c, &unverified);
    let b = render(&c, &verified);
    eprintln!(
        "  Stop -> done-unverified {} px, done-verified {} px in the attention band",
        attention_px(&a),
        attention_px(&b),
    );
    assert!(attention_px(&a) > 0 && attention_px(&b) > 0);
    assert!(
        attention_px(&a) > attention_px(&b),
        "needs review must not be quieter than finished"
    );
}

/// The verification evidence, which **Channel B cannot supply on its own.**
///
/// This is worth stating plainly because it is the one place the `done` split
/// crosses a channel boundary. `Stop` is a hook and raises the mark; whether the
/// mark is *verified* depends on a tool call having **completed successfully**,
/// and Channel B has no success event — Polis registers `PostToolUseFailure` and
/// deliberately not `PostToolUse`, which is the ~200 calls/sec firehose PRD §4.2
/// exists to keep off the hook channel.
///
/// So the test suite ran on Channel D (the transcript, which needs no setup at
/// all) or Channel A (OTel), and the session ended on Channel B. Two channels,
/// joined on the session id, and the split is only real when both are present.
fn ran_the_tests(world: &mut World, at: Instant) {
    let body = serde_json::json!({
        "type": "assistant",
        "cwd": CWD,
        "sessionId": SESSION,
        "message": {
            "role": "assistant",
            "stop_reason": "tool_use",
            "content": [{
                "type": "tool_use", "id": "toolu_test", "name": "Bash",
                "input": { "command": "cargo test --workspace" }
            }]
        }
    });
    world.apply(&transcript(TranscriptRecordKind::Assistant, body, at));
    let body = serde_json::json!({
        "type": "user",
        "cwd": CWD,
        "sessionId": SESSION,
        "message": {
            "role": "user",
            "content": [{ "type": "tool_result", "tool_use_id": "toolu_test", "content": "ok" }]
        }
    });
    world.apply(&transcript(
        TranscriptRecordKind::User,
        body,
        at + Duration::from_secs(1),
    ));
    world.tick(at + Duration::from_secs(1));
}

/// One transcript record, as `polis-ingest`'s tailer hands it over.
fn transcript(kind: TranscriptRecordKind, record: serde_json::Value, at: Instant) -> Event {
    let mut meta = EventMeta::now(Channel::Transcript).with_session(SessionId::new(SESSION));
    meta.observed = at;
    Event::new(
        meta,
        Payload::Transcript(Box::new(TranscriptEvent {
            kind,
            source: TranscriptSource::Main,
            byte_offset: 0,
            record,
        })),
    )
}

/// A `PreToolUse` on `Write`, the way the live capture delivered one.
fn write_a_file(world: &mut World, path: &LogicalPath, at: Instant) {
    feed(
        world,
        EventKind::PreToolUse,
        &format!(
            r#"{{{},"tool_name":"Write","tool_input":{{"file_path":"{CWD}/{path}","content":"fn main() {{}}\n"}},"tool_use_id":"toolu_02"}}"#,
            envelope("PreToolUse")
        ),
        at,
    );
}

/// `SubagentStop` is noise; a main thread going idle is news.
///
/// > **Main agents only.** `SubagentStop` is noise; a main thread going idle is
/// > news. (PRD §11.2b)
#[test]
fn a_subagent_stopping_raises_nothing() {
    let c = city();
    let mut world = world_for(&c);
    let t0 = Instant::now();
    feed(
        &mut world,
        EventKind::SubagentStart,
        &format!(
            r#"{{{},"agent_id":"agent-7","agent_type":"general-purpose"}}"#,
            envelope("SubagentStart")
        ),
        t0,
    );
    feed(
        &mut world,
        EventKind::SubagentStop,
        &format!(
            r#"{{{},"agent_id":"agent-7","agent_type":"general-purpose"}}"#,
            envelope("SubagentStop")
        ),
        t0 + Duration::from_secs(2),
    );
    assert!(
        world.attention.is_empty(),
        "a subagent finishing put a mark on the map: {:?}",
        world.attention
    );
}

/// The whole layer at once, on one map: what a live session actually produces,
/// and what M2 could not show at all.
#[test]
fn the_attention_layer_fires_on_a_map_from_hooks_alone() {
    let c = city();
    let mut world = world_for(&c);
    let t0 = Instant::now();
    let path = c
        .layout
        .buildings
        .keys()
        .next()
        .expect("a building")
        .clone();
    write_a_file(&mut world, &path, t0);
    feed(
        &mut world,
        EventKind::PermissionRequest,
        &format!(
            r#"{{{},"tool_name":"Bash","tool_input":{{"command":"git push --force"}}}}"#,
            envelope("PermissionRequest")
        ),
        t0 + Duration::from_secs(2),
    );
    // Five minutes later, still waiting: the escalation the corpus says a third
    // of real waits reach.
    world.tick(t0 + polis_world::attention::ESCALATION + Duration::from_secs(2));

    let (_, reader) = snapshot::from_world(&world);
    let snap = reader.load();
    let mut r = FrameRenderer::new(
        &c,
        FrameOptions {
            pixels: 480,
            supersample: 1,
            caption: false,
            rail: false,
            ..FrameOptions::default()
        },
    );
    let built = r.build_frame(&snap, Duration::from_millis(40));
    assert_eq!(built.attention.len(), 1, "the pin did not reach the frame");
    assert_eq!(built.attention[0].kind, MarkKind::NeedsDecision);
    assert!(
        built.attention[0].urgency > 0.99,
        "a five-minute-old pin is not escalated: {}",
        built.attention[0].urgency
    );
    let canvas = r.render_owned(&snap, Duration::from_millis(40));
    let px = attention_px(&canvas);
    eprintln!("  a live pin, five minutes ignored: {px} px in the attention band");
    assert!(px > 200, "the escalated pin inked only {px} px");

    if let Ok(dir) = std::env::var("POLIS_OUT") {
        let dir = std::path::Path::new(&dir);
        if std::fs::create_dir_all(dir).is_ok() {
            let _ = canvas.write_png(&dir.join("hook-pin.png"));
        }
    }
}
