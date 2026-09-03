//! Driving the world from the checked-in transcript fixtures (PRD §16).
//!
//! These run everywhere, including CI on a machine with no `~/.claude`. The
//! companion suite in `real_corpus.rs` runs the same code over the operator's
//! actual sessions and skips when there are none.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use polis_events::{LogicalPath, PathMapper, SessionId, ThreadId};
use polis_layout::CityLayout;
use polis_world::replay::{ReplayDriver, ReplaySchedule};
use polis_world::{snapshot, ThreadStatus, World};

/// The repository root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn fixtures() -> PathBuf {
    repo_root().join("tests/fixtures/transcripts")
}

/// Lays the two fixture files out the way Claude Code actually writes them:
/// `<munged-cwd>/<session-id>.jsonl` plus a sibling `<session-id>/subagents/`
/// directory (ADR-0013).
fn staged_session(dir: &Path) -> PathBuf {
    let session = "005d9938-2b44-45f6-87ac-4cddac3b0d6b";
    let project = dir.join("C--coding-demo-city");
    let subagents = project.join(session).join("subagents");
    std::fs::create_dir_all(&subagents).expect("staging dirs");
    std::fs::copy(
        fixtures().join("main-session-with-subagent.jsonl"),
        project.join(format!("{session}.jsonl")),
    )
    .expect("main transcript");
    std::fs::copy(
        fixtures().join("subagent-a96cf8a57447af436.jsonl"),
        subagents.join("agent-a96cf8a57447af436.jsonl"),
    )
    .expect("subagent transcript");
    std::fs::copy(
        fixtures().join("subagent-a96cf8a57447af436.meta.json"),
        subagents.join("agent-a96cf8a57447af436.meta.json"),
    )
    .expect("meta");
    project.join(session)
}

/// The `cwd` both fixture files record.
const FIXTURE_CWD: &str = r"C:\Users\user\AppData\Local\Temp\claude\C--coding-demo-city\6f51089f-1ec0-4e78-9bc9-8ff6864e0100\scratchpad\probe2";

fn mapper() -> PathMapper {
    PathMapper::new(Path::new(FIXTURE_CWD)).expect("fixture cwd is a usable root")
}

#[test]
fn a_whole_fixture_session_replays_into_a_consistent_world() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let session_dir = staged_session(tmp.path());
    let schedule =
        ReplaySchedule::from_session_dir(&session_dir, &mapper()).expect("offline full read");

    assert!(
        schedule.len() > 30,
        "the main transcript and the subagent file are both read: {} events",
        schedule.len()
    );
    assert!(
        schedule.files.len() >= 2,
        "a session is a forest, not a file (ADR-0013): {:?}",
        schedule.files.len()
    );

    let mut world = World::for_replay(CityLayout::default());
    let origin = Instant::now();
    let mut driver = ReplayDriver::with_origin(schedule, origin);
    let applied = driver.run_to_end(&mut world);
    assert_eq!(applied, driver.schedule().len());
    assert!(driver.is_finished());

    // One thread. The subagent's records carry the *parent's* session id
    // (ADR-0030), so keying on session alone would still give one thread — the
    // test that matters is that the worker is a worker, below.
    assert_eq!(
        world.threads.len(),
        1,
        "one session is one thread, subagents included"
    );
    let thread = world
        .thread(&ThreadId::of_session(SessionId::new(
            "005d9938-2b44-45f6-87ac-4cddac3b0d6b",
        )))
        .expect("the fixture session");

    assert_eq!(
        thread.workers.len(),
        1,
        "the subagent is a worker of the thread, not a thread of its own"
    );
    let worker = &thread.workers[0];
    assert_eq!(worker.id.as_str(), "a96cf8a57447af436");
    assert!(
        worker.attribution.is_attributed(),
        "the worker's parent link must be evidence, not a guess: {:?}",
        worker.attribution
    );
    assert!(
        world.unattributed.is_empty(),
        "nothing should be parked as unattributed here: {:?}",
        world.unattributed
    );

    // Every path the session touched is a file with live state.
    assert!(
        !world.files.is_empty(),
        "the session read and wrote files: {:?}",
        world.files.keys().collect::<Vec<_>>()
    );
    for (path, state) in &world.files {
        assert!(
            state.reads > 0 || state.writes > 0 || state.last_touched.is_some(),
            "{path} has state but no reason to"
        );
        assert_eq!(
            state.diff_lines,
            state.lines_added + state.lines_removed,
            "{path}: diff_lines must stay the sum it is defined as"
        );
        if state.writes > 0 {
            assert!(
                state.last_touched.is_some(),
                "{path} was written and must have a touch time"
            );
        }
    }

    // Trail and revisit counts agree with each other.
    assert!(!thread.trail.is_empty(), "a session leaves a trail");
    assert!(thread.trail.len() <= polis_world::TRAIL_CAP);
    for (path, _) in &thread.trail {
        assert!(
            thread.revisits(path) > 0,
            "{path} is on the trail but has no visit count"
        );
    }
    // The trail can hold one path more than once — that is the backtracking
    // PRD §12 wants visible — so the invariant is on the totals, not the
    // lengths.
    let steps: u32 = thread.visits.values().map(|v| v.count).sum();
    assert!(
        u64::from(steps) >= thread.trail.len() as u64,
        "every trail step was counted as a visit: {steps} counted, {} on the trail",
        thread.trail.len()
    );

    // The world is internally consistent: every toucher of a file is a thread.
    for (path, state) in &world.files {
        for toucher in &state.touched_by {
            assert!(
                world.threads.contains_key(toucher),
                "{path} names a thread that does not exist: {toucher}"
            );
        }
    }

    // Nothing drifted enough to matter, and nothing was dropped.
    assert!(world.health.dropped.is_empty());
    assert_eq!(world.health.events_applied, applied as u64);
}

#[test]
fn the_delegating_tool_is_agent_and_it_creates_a_worker() {
    // ADR-0031: there is no `Task` tool. A matcher on one silently matches
    // nothing, so this asserts the spawn actually landed.
    let tmp = tempfile::tempdir().expect("tempdir");
    let session_dir = staged_session(tmp.path());
    let schedule =
        ReplaySchedule::from_session_dir(&session_dir, &mapper()).expect("offline full read");
    let mut world = World::for_replay(CityLayout::default());
    let mut driver = ReplayDriver::with_origin(schedule, Instant::now());
    driver.run_to_end(&mut world);

    let thread = world.threads.values().next().expect("one thread");
    let worker = &thread.workers[0];
    assert!(
        !worker.kind.is_empty(),
        "the agent type is on the record; it must reach the worker"
    );
    assert_eq!(worker.kind.as_str(), "general-purpose");
}

#[test]
fn trails_decay_and_revisit_counts_do_not() {
    // PRD §12: trails persist and fade; the thrashing signal is the revisit
    // count, which is not a view and must survive the fade.
    let tmp = tempfile::tempdir().expect("tempdir");
    let session_dir = staged_session(tmp.path());
    let schedule =
        ReplaySchedule::from_session_dir(&session_dir, &mapper()).expect("offline full read");
    let origin = Instant::now();
    let mut world = World::for_replay(CityLayout::default());
    let mut driver = ReplayDriver::with_origin(schedule, origin);
    driver.run_to_end(&mut world);

    let thread = world.threads.values().next().expect("one thread");
    let before: Vec<(LogicalPath, u32)> = thread
        .visits
        .iter()
        .map(|(p, v)| (p.clone(), v.count))
        .collect();
    assert!(!before.is_empty());
    assert!(!thread.trail.is_empty());

    // An hour later, with no further events.
    world.tick(world.now() + Duration::from_secs(3_600));
    let thread = world.threads.values().next().expect("one thread");
    assert!(
        thread.trail.is_empty(),
        "the trail faded: {:?}",
        thread.trail
    );
    let after: Vec<(LogicalPath, u32)> = thread
        .visits
        .iter()
        .map(|(p, v)| (p.clone(), v.count))
        .collect();
    assert_eq!(before, after, "revisit counts are the signal, not the view");
    assert!(
        thread.territory.kernels.is_empty() && thread.territory.claim.is_none(),
        "and a dormant territory dissipates entirely (PRD §10.4)"
    );
    // The fixture's last two records are `assistant` / `stop_reason: end_turn`
    // with no tool call and no human reply after them, which is PRD §11.2's
    // primary state — the thread is parked on a human, not merely quiet. That
    // read used to be unavailable to a replay at all (the attention band was
    // 0.000% of map area in every frame of every M2 recording); it now comes
    // from `DecisionSource::TurnEnded`, and `Waiting` is what it looks like in
    // the status rail. An hour of silence does not resolve it: §11.2 says this
    // state "persists until resolved", and nobody has resolved it.
    assert_eq!(
        thread.status,
        ThreadStatus::Waiting,
        "parked on a human, which outranks 'alive but quiet'"
    );
}

#[test]
fn a_snapshot_is_what_the_renderer_sees_and_it_never_recomputes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let session_dir = staged_session(tmp.path());
    let schedule =
        ReplaySchedule::from_session_dir(&session_dir, &mapper()).expect("offline full read");
    let mut world = World::for_replay(CityLayout::default());
    let (publisher, reader) = snapshot::from_world(&world);

    let mut driver = ReplayDriver::with_origin(schedule, Instant::now());
    // A renderer sampling at its own cadence: publish once per "frame", after a
    // batch of events, never per event.
    let mut frames = 0;
    while !driver.is_finished() {
        for _ in 0..8 {
            if !driver.step(&mut world) {
                break;
            }
        }
        publisher.force(&world);
        frames += 1;
    }
    assert!(frames > 0);

    let snap = reader.load();
    assert_eq!(snap.threads.len(), world.threads.len());
    assert_eq!(snap.files.len(), world.files.len());
    assert_eq!(snap.generation, publisher.generation());

    // The reader holds a value, not a view: further mutation is invisible until
    // the next publish.
    world.file_entry_for_test();
    assert_eq!(
        reader.load().files.len(),
        snap.files.len(),
        "a reader cannot see, or force, work the publisher has not done"
    );
}

/// A tiny mutation helper, so the test above can prove the snapshot is a value.
trait TestMutate {
    fn file_entry_for_test(&mut self);
}

impl TestMutate for World {
    fn file_entry_for_test(&mut self) {
        let path = LogicalPath::new("tests/new-file.rs").expect("path");
        self.files.insert(path, polis_world::FileState::default());
    }
}
