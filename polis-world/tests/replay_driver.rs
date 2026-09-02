//! The M2 transport: play, pause, speed, seek, step, and jump to the next
//! interesting moment (PRD §15).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use polis_events::{
    Channel, Event, EventMeta, FsEvent, LogicalPath, PathMapper, Payload, RecordedEvent,
    RecordingHeader, SessionId, WallTime, WorktreeId, RECORDING_FORMAT,
};
use polis_ingest::transcript::ParseStats;
use polis_layout::CityLayout;
use polis_world::replay::{
    Interest, ReplayDriver, ReplaySchedule, DEFAULT_IDLE_GAP_CAP, SPEED_RANGE,
};
use polis_world::World;

fn recorded(offset_ms: u64, path: &str) -> RecordedEvent {
    let meta = EventMeta::now(Channel::Fs).with_session(SessionId::new("s1"));
    let payload = Payload::Fs(FsEvent::Modified {
        path: (WorktreeId::PRIMARY, LogicalPath::new(path).unwrap()),
    });
    RecordedEvent {
        wall: WallTime::from_unix_millis(i64::try_from(offset_ms).unwrap()),
        monotonic_offset_ms: offset_ms,
        event: Event::new(meta, payload),
    }
}

fn schedule(offsets: &[u64]) -> ReplaySchedule {
    let events = offsets
        .iter()
        .enumerate()
        .map(|(i, o)| recorded(*o, &format!("src/f{i}.rs")))
        .collect();
    ReplaySchedule::from_events(
        RecordingHeader {
            format: RECORDING_FORMAT,
            wall_origin: WallTime::UNIX_EPOCH,
            producer: "test".to_owned(),
        },
        events,
        ParseStats::default(),
        Vec::new(),
    )
}

/// A comparable summary of everything the world holds.
fn fingerprint(world: &World) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (id, t) in &world.threads {
        let _ = writeln!(
            out,
            "thread {id} status={} tools={} trail={} visits={} workers={} claim={:?}",
            t.status,
            t.tool_calls,
            t.trail.len(),
            t.visits.len(),
            t.workers.len(),
            t.territory.claim.as_ref().map(LogicalPath::as_str),
        );
    }
    for (path, f) in &world.files {
        let _ = writeln!(
            out,
            "file {path} diff={} reads={} writes={} precision={:?}",
            f.diff_lines, f.reads, f.writes, f.diff_precision
        );
    }
    let _ = writeln!(out, "attention={}", world.attention.len());
    out
}

#[test]
fn the_driver_applies_events_as_the_clock_crosses_them() {
    let mut world = World::for_replay(CityLayout::default());
    let mut driver = ReplayDriver::with_origin(schedule(&[0, 1_000, 2_000]), Instant::now());
    driver.clock_mut().play();

    assert_eq!(driver.advance(Duration::from_millis(1), &mut world), 1);
    assert_eq!(driver.advance(Duration::from_millis(500), &mut world), 0);
    assert_eq!(driver.advance(Duration::from_millis(600), &mut world), 1);
    assert_eq!(driver.advance(Duration::from_secs(5), &mut world), 1);
    assert!(driver.is_finished());
    assert_eq!(driver.progress().events_applied, 3);
}

#[test]
fn speed_scales_playback_and_is_clamped_at_both_ends() {
    let mut world = World::for_replay(CityLayout::default());
    let mut driver = ReplayDriver::with_origin(schedule(&[0, 8_000, 16_000]), Instant::now());
    driver.clock_mut().play();
    driver.clock_mut().set_speed(64.0);
    // One real second at 64x crosses 64 seconds of the schedule.
    assert_eq!(driver.advance(Duration::from_secs(1), &mut world), 3);

    let mut clock = driver.clock().clone();
    clock.set_speed(1e9);
    assert!((clock.speed() - SPEED_RANGE.1).abs() < f32::EPSILON);
    clock.set_speed(-4.0);
    assert!((clock.speed() - SPEED_RANGE.0).abs() < f32::EPSILON);
}

#[test]
fn stepping_one_event_works_with_the_clock_paused() {
    let mut world = World::for_replay(CityLayout::default());
    let mut driver = ReplayDriver::with_origin(schedule(&[0, 1_000, 2_000]), Instant::now());
    assert!(!driver.clock().is_playing());
    assert!(driver.step(&mut world));
    assert_eq!(driver.progress().events_applied, 1);
    assert_eq!(driver.clock().position_ms(), 0);
    assert!(driver.step(&mut world));
    assert_eq!(driver.clock().position_ms(), 1_000);
    assert!(driver.step(&mut world));
    assert!(!driver.step(&mut world), "the end is the end");
}

#[test]
fn seeking_backwards_rebuilds_the_world_exactly() {
    // Events are not invertible, so a backwards seek resets and re-applies.
    // The proof that this is right is that the state matches.
    let origin = Instant::now();
    let mut world = World::for_replay(CityLayout::default());
    let mut driver = ReplayDriver::with_origin(schedule(&[0, 1_000, 2_000, 3_000]), origin);

    driver.seek(2_000, &mut world);
    let at_two = fingerprint(&world);
    assert_eq!(driver.progress().events_applied, 3);

    driver.seek(3_000, &mut world);
    let at_three = fingerprint(&world);
    assert_ne!(at_two, at_three);

    driver.seek(2_000, &mut world);
    assert_eq!(
        fingerprint(&world),
        at_two,
        "a backwards seek must land on exactly the state that moment had"
    );
    assert_eq!(driver.progress().events_applied, 3);

    driver.seek(0, &mut world);
    assert_eq!(driver.progress().events_applied, 1);
    driver.seek_fraction(1.0, &mut world);
    assert_eq!(fingerprint(&world), at_three);
}

#[test]
fn the_scrubber_reports_both_timelines() {
    let mut world = World::for_replay(CityLayout::default());
    let compressed = schedule(&[0, 100, 2_400_000]).with_idle_gap_cap(Some(DEFAULT_IDLE_GAP_CAP));
    let mut driver = ReplayDriver::with_origin(compressed, Instant::now());

    let p = driver.progress();
    assert_eq!(
        p.duration_ms, 2_100,
        "playback: 100 ms of work plus a 2 s cap"
    );
    assert_eq!(p.session_duration_ms, 2_400_000, "session: forty minutes");
    assert_eq!(p.events_total, 3);
    assert!((p.fraction()).abs() < f32::EPSILON);

    driver.seek_fraction(1.0, &mut world);
    let p = driver.progress();
    assert!((p.fraction() - 1.0).abs() < f32::EPSILON);
    assert_eq!(p.events_applied, 3);
    assert_eq!(p.session_elapsed(), Duration::from_mins(40));
    assert!(p.wall.is_some(), "a scrubber wants a date on the label");
}

#[test]
fn jumping_to_the_next_interesting_moment_crosses_the_dead_air() {
    let mut world = World::for_replay(CityLayout::default());
    // Three bursts, half an hour apart.
    let compressed = schedule(&[0, 50, 1_800_000, 1_800_050, 3_600_000])
        .with_idle_gap_cap(Some(DEFAULT_IDLE_GAP_CAP));
    let mut driver = ReplayDriver::with_origin(compressed, Instant::now());

    assert_eq!(
        driver.seek_next_interesting(&mut world),
        Some(Interest::Burst)
    );
    assert_eq!(driver.progress().events_applied, 3, "landed on the burst");
    assert_eq!(
        driver.seek_next_interesting(&mut world),
        Some(Interest::Burst)
    );
    assert_eq!(driver.progress().events_applied, 5);
    assert_eq!(driver.seek_next_interesting(&mut world), None);
}

#[test]
fn a_world_ages_by_real_session_time_across_a_compressed_gap() {
    // The whole reason the two timelines are separate: the operator watches two
    // seconds, and the world lives through forty minutes of decay.
    let origin = Instant::now();
    let mut world = World::for_replay(CityLayout::default());
    let compressed = schedule(&[0, 2_400_000]).with_idle_gap_cap(Some(DEFAULT_IDLE_GAP_CAP));
    let mut driver = ReplayDriver::with_origin(compressed, origin);
    driver.clock_mut().play();

    driver.advance(Duration::from_millis(1), &mut world);
    let early = world.now();
    driver.advance(Duration::from_millis(1_000), &mut world);
    let midway = world.now();
    let aged = midway.saturating_duration_since(early);
    assert!(
        aged > Duration::from_secs(600),
        "one second of watching crossed {aged:?} of session time"
    );
}

/// The workspace root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn open_reads_a_main_transcript_together_with_its_subagents() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let fixtures = repo_root().join("tests/fixtures/transcripts");
    let session = "005d9938-2b44-45f6-87ac-4cddac3b0d6b";
    let project = tmp.path().join("C--coding-demo-city");
    let subagents = project.join(session).join("subagents");
    std::fs::create_dir_all(&subagents).unwrap();
    let main = project.join(format!("{session}.jsonl"));
    std::fs::copy(fixtures.join("main-session-with-subagent.jsonl"), &main).unwrap();
    std::fs::copy(
        fixtures.join("subagent-a96cf8a57447af436.jsonl"),
        subagents.join("agent-a96cf8a57447af436.jsonl"),
    )
    .unwrap();

    let mapper = PathMapper::new(tmp.path()).expect("mapper");
    // Given the *main transcript*, `open` must still find the subagent files —
    // 79% of transcript files are subagent files, and a replay without them
    // shows an orchestrator doing nothing (ADR-0013).
    let from_main = ReplaySchedule::open(&main, &mapper).expect("open");
    assert!(
        from_main.files.len() >= 2,
        "expected the forest, got {:?}",
        from_main.files
    );
    // Given the sidecar directory, the same answer.
    let from_dir = ReplaySchedule::open(&project.join(session), &mapper).expect("open");
    assert_eq!(from_main.len(), from_dir.len());

    // Given a subagent file on its own, only that file.
    let one = ReplaySchedule::open(&subagents.join("agent-a96cf8a57447af436.jsonl"), &mapper)
        .expect("open");
    assert_eq!(one.files.len(), 1);
    assert!(one.len() < from_main.len());
}
