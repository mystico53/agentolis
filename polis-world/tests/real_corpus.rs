//! Driving the world from the operator's **real** sessions (PRD §15, §16).
//!
//! > TEST with real data, not mocks.
//!
//! Every test here skips with a printed reason when `~/.claude/projects` does
//! not exist, so CI on a bare runner stays green while a developer machine gets
//! the real coverage. Run with `-- --nocapture` to see the numbers.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use polis_events::{LogicalPath, PathMapper};
use polis_layout::CityLayout;
use polis_world::replay::{ReplayDriver, DEFAULT_IDLE_GAP_CAP};
use polis_world::sessions::{IndexDepth, IndexOptions, SessionIndex, SessionSummary};
use polis_world::{DenylistUbiquity, World};

/// `~/.claude/projects`, or `None` on a machine that has never run Claude Code.
fn projects_dir() -> Option<PathBuf> {
    polis_world::sessions::default_projects_dir().filter(|p| p.is_dir())
}

/// Counts the main transcripts on disk the way a shell would: one `*.jsonl`
/// directly inside each project directory.
fn main_transcripts_on_disk(projects: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(projects) else {
        return out;
    };
    for project in entries.flatten() {
        if !project.path().is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
                out.insert(path);
            }
        }
    }
    out
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn the_session_index_matches_what_is_on_disk() {
    let Some(projects) = projects_dir() else {
        eprintln!("skipped: no ~/.claude/projects on this machine");
        return;
    };

    let started = Instant::now();
    let index = SessionIndex::scan_with(&projects, &IndexOptions::quick()).expect("scan");
    let elapsed = started.elapsed();

    let on_disk = main_transcripts_on_disk(&projects);
    eprintln!(
        "indexed {} sessions in {:?} (headers only); {} main transcripts on disk",
        index.len(),
        elapsed,
        on_disk.len()
    );
    assert_eq!(
        index.len(),
        on_disk.len(),
        "the index must find exactly the main transcripts that exist"
    );

    let indexed: BTreeSet<PathBuf> = index
        .sessions
        .iter()
        .map(|s| s.transcript.clone())
        .collect();
    assert_eq!(indexed, on_disk);

    for s in &index.sessions {
        assert!(
            s.transcript.is_file(),
            "{} vanished",
            s.transcript.display()
        );
        assert_eq!(
            s.transcript.file_stem().and_then(|f| f.to_str()),
            Some(s.session.as_str()),
            "the file stem is the session id on 808 of 808 files"
        );
        assert_eq!(
            s.repo_exists,
            s.repo.as_deref().is_some_and(Path::is_dir),
            "{}: repo_exists must be checked against disk, not remembered",
            s.session
        );
        if let Some(dir) = &s.sidecar_dir {
            assert!(dir.is_dir());
        }
        assert!(
            s.branch.as_deref() != Some("HEAD"),
            "a detached head is not a branch (ADR-0018)"
        );
    }

    // Most recent first.
    let keys: Vec<i64> = index
        .sessions
        .iter()
        .map(|s| {
            s.ended
                .map_or(s.modified_ms, polis_events::WallTime::unix_millis)
        })
        .collect();
    assert!(
        keys.windows(2).all(|w| w[0] >= w[1]),
        "the picker shows the newest session first"
    );

    let repos = index.repositories();
    eprintln!("  across {} repositories", repos.len());
    for (repo, n) in repos.iter().take(5) {
        eprintln!("    {n:>4}  {}", repo.display());
    }
    let replayable = index.replayable().count();
    eprintln!(
        "  {replayable} replayable right now ({} whose repository is gone)",
        index.len() - replayable
    );
}

#[test]
fn a_full_index_counts_tool_calls_and_caches_them() {
    let Some(projects) = projects_dir() else {
        eprintln!("skipped: no ~/.claude/projects on this machine");
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let options = IndexOptions {
        depth: IndexDepth::Full,
        cache: Some(tmp.path().join("sessions-index.json")),
        limit: None,
    };

    let cold = Instant::now();
    let first = SessionIndex::scan_with(&projects, &options).expect("cold scan");
    let cold = cold.elapsed();
    let warm = Instant::now();
    let second = SessionIndex::scan_with(&projects, &options).expect("warm scan");
    let warm = warm.elapsed();

    let bytes: u64 = first.sessions.iter().map(|s| s.bytes).sum();
    let tool_calls: u64 = first.sessions.iter().map(|s| u64::from(s.tool_calls)).sum();
    let records: u64 = first.sessions.iter().map(|s| s.records).sum();
    let subagents: u64 = first.sessions.iter().map(|s| u64::from(s.subagents)).sum();
    #[allow(clippy::cast_precision_loss)] // a byte count, for one printed line
    let megabytes = bytes as f64 / 1_048_576.0;
    eprintln!(
        "full scan: {} sessions, {megabytes:.1} MB, {records} records, {tool_calls} tool calls, \
         {subagents} subagent transcripts",
        first.len(),
    );
    eprintln!("  cold {cold:?}, warm (cached) {warm:?}");
    assert_eq!(first.scanned, first.len(), "a cold scan reads every file");
    assert_eq!(second.from_cache, second.len(), "a warm scan reads none");
    assert_eq!(second.scanned, 0);
    assert!(
        warm < cold || cold < Duration::from_millis(50),
        "the cache must be the fast path: cold {cold:?}, warm {warm:?}"
    );

    // Cross-check one session's tool-call count against a real JSON parse.
    let Some(sample) = first
        .sessions
        .iter()
        .filter(|s| s.tool_calls > 0 && s.bytes < 4 * 1024 * 1024)
        .max_by_key(|s| s.tool_calls)
    else {
        eprintln!("  no session small enough to cross-check");
        return;
    };
    let text = std::fs::read_to_string(&sample.transcript).expect("read");
    let mut parsed = 0_u32;
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(blocks) = value
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                parsed += 1;
                if let Some(p) = block
                    .get("input")
                    .and_then(|i| i.get("file_path"))
                    .and_then(|p| p.as_str())
                {
                    paths.insert(p.to_ascii_lowercase().replace('\\', "/"));
                }
            }
        }
    }
    eprintln!(
        "  cross-check {}: byte scan {} tool calls / {} files, JSON parse {parsed} / {}",
        sample.session,
        sample.tool_calls,
        sample.files_touched,
        paths.len()
    );
    assert_eq!(
        sample.tool_calls, parsed,
        "the byte scan must agree with a real parse"
    );
    assert!(
        sample.files_touched >= u32::try_from(paths.len()).unwrap(),
        "the scan sees `file_path` outside `tool_use.input` too, never fewer"
    );
}

/// The largest replayable session for this repository, if the operator has one.
///
/// Sized on bytes rather than on the record count, because these tests index at
/// [`IndexDepth::Headers`] and that depth deliberately does not count records.
fn sample_session(index: &SessionIndex) -> Option<SessionSummary> {
    let root = repo_root();
    let big = |s: &&SessionSummary| s.is_replayable() && s.bytes > 64 * 1024;
    index
        .for_repo(&root)
        .filter(big)
        .max_by_key(|s| s.bytes)
        .cloned()
        .or_else(|| {
            index
                .replayable()
                .filter(big)
                .max_by_key(|s| s.bytes)
                .cloned()
        })
}

#[test]
#[allow(clippy::too_many_lines)] // one assertion per invariant; splitting hides the set
fn a_real_session_drives_a_consistent_world_over_a_real_city() {
    let Some(projects) = projects_dir() else {
        eprintln!("skipped: no ~/.claude/projects on this machine");
        return;
    };
    let index = SessionIndex::scan_with(&projects, &IndexOptions::quick()).expect("scan");
    let Some(sample) = sample_session(&index) else {
        eprintln!("skipped: no replayable session with a live repository");
        return;
    };
    let repo_path = sample
        .repo
        .clone()
        .expect("a replayable session has a repo");
    eprintln!(
        "replaying {} ({} records, {} bytes) over {}",
        sample.session,
        sample.records,
        sample.bytes,
        repo_path.display()
    );

    // A real city, from the real repository the session ran in.
    let built = Instant::now();
    let repo = polis_repo::tree::RepoIndex::open(&repo_path).expect("index the repository");
    let layout = polis_layout::city::generate(repo.tree());
    eprintln!(
        "  city: {} buildings, {} districts, built in {:?}",
        layout.buildings.len(),
        layout.districts.len(),
        built.elapsed()
    );

    let mapper = PathMapper::new(&repo_path).expect("mapper");
    let read = Instant::now();
    let schedule = sample.schedule(&mapper).expect("offline full read");
    eprintln!(
        "  read {} events from {} files in {:?} ({} skipped lines)",
        schedule.len(),
        schedule.files.len(),
        read.elapsed(),
        schedule.stats.drift()
    );
    assert!(!schedule.is_empty(), "a real session produces events");

    let mut world = World::new(repo.tree().clone(), layout);
    world.set_ubiquity(Box::new(DenylistUbiquity::default()));
    let origin = Instant::now();
    let mut driver = ReplayDriver::with_origin(schedule, origin);
    let applied_at = Instant::now();
    let applied = driver.run_to_end(&mut world);
    let apply_time = applied_at.elapsed();
    #[allow(clippy::cast_precision_loss)] // an event count, for one printed line
    let per_event_us = apply_time.as_secs_f64() * 1e6 / applied.max(1) as f64;
    eprintln!("  applied {applied} events in {apply_time:?} ({per_event_us:.1} µs/event)");

    // --- consistency, over a whole real session ---------------------------
    assert!(
        !world.threads.is_empty(),
        "a session is at least one thread"
    );
    for (id, thread) in &world.threads {
        assert_eq!(&thread.id, id, "threads are keyed by their own id");
        assert!(thread.trail.len() <= polis_world::TRAIL_CAP);
        assert!(thread.ops.len() <= polis_world::OPS_CAP);
        for (path, _) in &thread.trail {
            assert!(thread.visits.contains_key(path));
        }
        for worker in &thread.workers {
            assert!(
                worker.attribution.is_attributed(),
                "{}: worker {} has no evidence for its parent",
                id,
                worker.id
            );
            assert!(worker.last_activity >= worker.started);
        }
        let ids: BTreeSet<&str> = thread.workers.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids.len(), thread.workers.len(), "workers are unique");
        // Territory: converged or not, it must be self-consistent.
        let convergence = thread.territory.convergence();
        if let Some(claim) = &thread.territory.claim {
            assert!(
                claim.depth() >= polis_world::territory::MIN_CLAIM_DEPTH,
                "{id}: a claim below depth 2 must never be emitted, got {claim}"
            );
        } else {
            assert!(
                convergence.claim.is_none() || thread.territory.evidence.is_empty(),
                "{id}: a converged territory must carry its claim"
            );
        }
        for kernel in &thread.territory.kernels {
            assert!(kernel.weight > 0.0 && kernel.weight.is_finite());
            assert!(kernel.centre.is_finite());
            assert!(kernel.radius > 0.0);
        }
    }

    for (path, state) in &world.files {
        assert_eq!(state.diff_lines, state.lines_added + state.lines_removed);
        for toucher in &state.touched_by {
            assert!(
                world.threads.contains_key(toucher),
                "{path} names a thread that does not exist"
            );
        }
        if state.diff_lines > 0 {
            assert_ne!(
                state.diff_precision,
                polis_world::DiffPrecision::Unknown,
                "{path} has a diff and must say how it was measured"
            );
        }
    }

    for mark in &world.attention {
        assert!(
            world.threads.contains_key(mark.thread()),
            "an attention mark must belong to a live thread"
        );
    }
    // PRD §11.1 ordering holds after every tick.
    let ranks: Vec<u8> = world.attention.iter().map(|m| m.kind.rank()).collect();
    assert!(ranks.windows(2).all(|w| w[0] <= w[1]));

    // --- what it actually saw ---------------------------------------------
    let placed = world
        .threads
        .values()
        .filter(|t| t.territory.claim.is_some())
        .count();
    let workers: usize = world.threads.values().map(|t| t.workers.len()).sum();
    let mutated = world.files.values().filter(|f| f.writes > 0).count();
    eprintln!(
        "  {} threads ({placed} with a territory), {workers} workers, {} files ({mutated} written)",
        world.threads.len(),
        world.files.len()
    );
    eprintln!(
        "  health: drift {}, unmapped paths {}, unplaced observations {}, \
         at-mentions {}, unattributed workers {} (map holds {})",
        world.health.drift,
        world.health.unmapped_paths,
        world.health.unplaced_observations,
        world.health.at_mentions,
        world.health.unattributed_workers,
        world.unattributed.len(),
    );
    assert_eq!(
        world.health.unattributed_workers,
        world.unattributed.len() as u64,
        "the counter must report what is unplaced now, not a high-water mark"
    );
    for w in world.unattributed.values() {
        eprintln!(
            "    unplaced worker {} ({} records): {}",
            w.id, w.records, w.reason
        );
    }

    // The city is real, so a real session must land on real buildings.
    let on_a_building = world
        .files
        .keys()
        .filter(|p| world.position_of(p).is_some())
        .count();
    eprintln!(
        "  {on_a_building} of {} touched files have a building on this city",
        world.files.len()
    );
    if world.repo.root == repo_path {
        assert!(
            on_a_building > 0,
            "a session that ran in this repository must touch buildings in its city"
        );
    }
}

#[test]
fn seeking_a_real_session_lands_on_the_state_that_moment_had() {
    let Some(projects) = projects_dir() else {
        eprintln!("skipped: no ~/.claude/projects on this machine");
        return;
    };
    let index = SessionIndex::scan_with(&projects, &IndexOptions::quick()).expect("scan");
    let Some(sample) = sample_session(&index) else {
        eprintln!("skipped: no replayable session with a live repository");
        return;
    };
    let repo_path = sample.repo.clone().expect("repo");
    let mapper = PathMapper::new(&repo_path).expect("mapper");
    let schedule = sample
        .schedule(&mapper)
        .expect("offline full read")
        .with_idle_gap_cap(Some(DEFAULT_IDLE_GAP_CAP));
    eprintln!(
        "{}: {} events, session {:?}, watchable in {:?} ({:?} of dead air removed)",
        sample.session,
        schedule.len(),
        Duration::from_millis(schedule.session_duration_ms()),
        Duration::from_millis(schedule.duration_ms()),
        Duration::from_millis(schedule.compressed_ms()),
    );

    let origin = Instant::now();
    let mut world = World::for_replay(CityLayout::default());
    let mut driver = ReplayDriver::with_origin(schedule, origin);

    let halfway = driver.schedule().duration_ms() / 2;
    driver.seek(halfway, &mut world);
    let forward = fingerprint(&world);
    let applied_forward = driver.progress().events_applied;

    driver.seek(driver.schedule().duration_ms(), &mut world);
    let at_end = fingerprint(&world);
    assert_ne!(forward, at_end, "a real session changes as it runs");

    driver.seek(halfway, &mut world);
    assert_eq!(
        fingerprint(&world),
        forward,
        "a backwards seek over a real session must reproduce the state exactly"
    );
    assert_eq!(driver.progress().events_applied, applied_forward);

    driver.seek(0, &mut world);
    driver.run_to_end(&mut world);
    assert_eq!(
        fingerprint(&world),
        at_end,
        "and replaying from the start reproduces the end"
    );
    eprintln!("  seek/reset/replay reproduced both checkpoints exactly");
}

#[test]
fn interesting_moments_are_findable_in_a_real_session() {
    let Some(projects) = projects_dir() else {
        eprintln!("skipped: no ~/.claude/projects on this machine");
        return;
    };
    let index = SessionIndex::scan_with(&projects, &IndexOptions::quick()).expect("scan");
    let Some(sample) = sample_session(&index) else {
        eprintln!("skipped: no replayable session with a live repository");
        return;
    };
    let mapper = PathMapper::new(sample.repo.as_ref().expect("repo")).expect("mapper");
    let schedule = sample
        .schedule(&mapper)
        .expect("offline full read")
        .with_idle_gap_cap(Some(DEFAULT_IDLE_GAP_CAP));
    let total = schedule.len();
    let marks: Vec<_> = schedule.interesting().collect();
    eprintln!(
        "{}: {} of {total} events are worth stopping at",
        sample.session,
        marks.len()
    );
    assert!(
        marks.len() < total,
        "if everything is interesting, nothing is"
    );

    let mut world = World::for_replay(CityLayout::default());
    let mut driver = ReplayDriver::with_origin(schedule, Instant::now());
    let mut jumps = 0;
    while driver.seek_next_interesting(&mut world).is_some() {
        jumps += 1;
        // One jump consumes at least one mark — several when marks share a
        // millisecond, which a fan-out of subagents does routinely.
        assert!(jumps <= marks.len(), "each jump must consume a mark");
    }
    assert!(jumps > 0);
    assert!(
        driver.is_finished(),
        "the jumps must cross the whole session"
    );
    eprintln!(
        "  {jumps} jumps crossed the whole session ({} marks consumed)",
        marks.len()
    );
}

/// A comparable summary of everything the world holds.
fn fingerprint(world: &World) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (id, t) in &world.threads {
        let _ = writeln!(
            out,
            "thread {id} status={} tools={} fail={} trail={} visits={} workers={} claim={:?}              kernels={} added={} removed={}",
            t.status,
            t.tool_calls,
            t.failures,
            t.trail.len(),
            t.visits.len(),
            t.workers.len(),
            t.territory.claim.as_ref().map(LogicalPath::as_str),
            t.territory.kernels.len(),
            t.lines_added,
            t.lines_removed,
        );
        for w in &t.workers {
            let _ = writeln!(
                out,
                "  worker {} kind={} running={} ops={} attribution={:?}",
                w.id, w.kind, w.running, w.ops, w.attribution
            );
        }
    }
    for (path, f) in &world.files {
        let _ = writeln!(
            out,
            "file {path} diff={} reads={} writes={} precision={:?}",
            f.diff_lines, f.reads, f.writes, f.diff_precision
        );
    }
    let _ = writeln!(
        out,
        "attention={} claims={} unattributed={}",
        world.attention.len(),
        world.claims.len(),
        world.unattributed.len()
    );
    out
}
