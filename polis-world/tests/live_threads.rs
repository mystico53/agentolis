//! Many threads at once, live (PRD §15 M4).
//!
//! > **M4 — Multi-thread + territories + clouds.** Territory inference, KDE,
//! > iso-contour rendering, tethers to workers.
//!
//! M3 proved one live session could reach the map. This file is about the plural
//! case, and about the operator's actual sentence — *"i want to see all agents
//! active in a repository on the machine"* — which is plural on purpose: agents
//! they started themselves, in their own terminals, several at once.
//!
//! # What is exercised, and why through files
//!
//! The multi-session tests drive the **whole chain** rather than a synthesised
//! event stream: real JSONL bytes are appended to real transcript files under a
//! temporary `~/.claude/projects`, discovered by
//! [`polis_ingest::live::LiveTailer`], and applied to one [`World`]. That is the
//! zero-configuration path — no hooks, no telemetry, no wrapper — and it is the
//! only one that can be wrong in the ways this milestone cares about:
//! attributing a worker to the wrong thread, tailing a session that is not in
//! this repository, or never noticing a session that started after the watch.
//!
//! The territory, hysteresis and lifecycle tests take the other road and build
//! [`polis_events::Event`]s directly, because they are about **time** — a 90 s
//! half-life, eight sustained observations, a thirty-minute silence — and a test
//! that had to spend that time would not be a test anyone runs.
//!
//! The companion acceptance test that starts *real* `claude` processes is
//! `several_real_agents.rs`, which is `#[ignore]`d because it costs an
//! operator's API quota.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use polis_events::{
    Channel, Event, EventMeta, LogicalPath, PathMapper, Payload, SessionId, ThreadId,
    TranscriptEvent, TranscriptRecordKind, TranscriptSource, WorkerId,
};
use polis_repo::{FileMeta, RepoTree};
use polis_world::territory::{DECAY_HALF_LIFE, DRIFT_CONFIRMATIONS};
use polis_world::{
    ThreadStatus, WorkerAttribution, World, MAX_THREADS, THREAD_RETIRE_AFTER, UNATTRIBUTED_TTL,
};

// ---------------------------------------------------------------------------
// A fleet of sessions on disk
// ---------------------------------------------------------------------------

/// A temporary `~/.claude/projects` plus the checkout its sessions ran in.
struct Fleet {
    _tmp: tempfile::TempDir,
    projects: PathBuf,
    repo: PathBuf,
    project_dir: PathBuf,
}

impl Fleet {
    /// Lays out an empty projects directory and an empty checkout beside it.
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&projects).expect("projects dir");
        std::fs::create_dir_all(repo.join("src/auth")).expect("repo dirs");
        std::fs::create_dir_all(repo.join("src/render")).expect("repo dirs");
        std::fs::create_dir_all(repo.join("docs")).expect("repo dirs");
        // The munged directory name is opaque to Polis — it is *discovered*, not
        // computed (ADR-0033) — so any stable name will do here.
        let project_dir = projects.join("temp--polis-m4");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        Self {
            _tmp: tmp,
            projects,
            repo,
            project_dir,
        }
    }

    /// The `cwd` every record in this fleet reports.
    fn cwd(&self) -> String {
        self.repo.display().to_string()
    }

    /// An absolute path inside the checkout, as a record would spell it.
    fn file(&self, rel: &str) -> String {
        self.repo.join(rel).display().to_string()
    }

    /// A session that has not written anything yet.
    fn session(&self, id: &str) -> Session {
        Session {
            id: id.to_owned(),
            main: self.project_dir.join(format!("{id}.jsonl")),
            sidecar: self.project_dir.join(id),
            cwd: self.cwd(),
            seq: std::cell::Cell::new(0),
        }
    }

    /// A tailer watching this checkout, started before any session exists.
    fn watch(&self) -> polis_ingest::live::LiveTailer {
        let mapper = PathMapper::new(&self.repo).expect("the checkout is a usable root");
        polis_ingest::live::LiveTailer::open(
            &self.projects,
            polis_ingest::live::Scope::Repo(mapper),
        )
    }

    /// A world over a small city built from this checkout's directories.
    fn world(&self) -> World {
        let mut tree = RepoTree {
            root: self.repo.clone(),
            ..RepoTree::default()
        };
        for (path, size) in [
            ("src/auth/token.rs", 4_000),
            ("src/auth/session.rs", 3_000),
            ("src/auth/login.rs", 2_500),
            ("src/render/frame.rs", 5_000),
            ("src/render/camera.rs", 2_000),
            ("docs/README.md", 1_000),
        ] {
            let logical = LogicalPath::new(path).expect("test path");
            tree.files
                .insert(logical.clone(), FileMeta::untracked(logical, size));
        }
        let layout = polis_layout::city::generate(&tree);
        World::new(tree, layout)
    }
}

/// One session's transcript files.
struct Session {
    id: String,
    main: PathBuf,
    sidecar: PathBuf,
    cwd: String,
    seq: std::cell::Cell<u32>,
}

impl Session {
    /// Appends one record to the main transcript.
    fn main_record(&self, value: serde_json::Value) {
        append(&self.main, &self.envelope(value, None));
    }

    /// Appends one record to a subagent transcript, creating the file the way
    /// Claude Code does: `<session>/subagents/agent-<id>.jsonl` (ADR-0013).
    fn worker_record(&self, worker: &str, value: serde_json::Value) {
        let dir = self.sidecar.join("subagents");
        std::fs::create_dir_all(&dir).expect("subagents dir");
        let path = dir.join(format!("agent-{worker}.jsonl"));
        append(&path, &self.envelope(value, Some(worker)));
    }

    /// Wraps a body in the envelope every threaded record carries
    /// (`docs/verified/jsonl-schema.md` §3).
    fn envelope(&self, mut value: serde_json::Value, worker: Option<&str>) -> serde_json::Value {
        let n = self.seq.get();
        self.seq.set(n + 1);
        value["uuid"] = serde_json::json!(format!("{}-{n}", self.id));
        value["sessionId"] = serde_json::json!(self.id);
        value["cwd"] = serde_json::json!(self.cwd);
        value["gitBranch"] = serde_json::json!("main");
        value["version"] = serde_json::json!("2.1.248");
        if let Some(w) = worker {
            value["agentId"] = serde_json::json!(w);
            value["isSidechain"] = serde_json::json!(true);
        }
        value
    }
}

/// One `tool_use` block, as an assistant record carries it.
fn tool_use(tool: &str, id: &str, input: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [{ "type": "tool_use", "id": id, "name": tool, "input": input }],
        },
    })
}

/// An `Edit` of one absolute path.
fn edit(path: &str, id: &str) -> serde_json::Value {
    tool_use(
        "Edit",
        id,
        &serde_json::json!({ "file_path": path, "old_string": "a", "new_string": "b" }),
    )
}

/// A `Read` of one absolute path.
fn read(path: &str, id: &str) -> serde_json::Value {
    tool_use("Read", id, &serde_json::json!({ "file_path": path }))
}

fn append(path: &Path, value: &serde_json::Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent dir");
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open transcript");
    writeln!(file, "{value}").expect("append record");
}

/// Polls until `want` events have been read or the deadline passes.
///
/// The tailer is a polling reader by design (PRD §13.1's idle budget), so a test
/// that read once would be racing the filesystem rather than testing anything.
fn drain(tailer: &mut polis_ingest::live::LiveTailer, world: &mut World, want: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut applied = 0usize;
    while applied < want && Instant::now() < deadline {
        tailer.rescan();
        let mut events = Vec::new();
        tailer.poll(&mut events);
        for event in &events {
            world.apply(event);
        }
        applied += events.len();
        world.tick(Instant::now());
        if events.is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    applied
}

// ---------------------------------------------------------------------------
// 1. Many concurrent threads
// ---------------------------------------------------------------------------

#[test]
fn three_sessions_in_one_repository_become_three_threads_with_their_own_workers() {
    let fleet = Fleet::new();
    // The watch starts first, with nothing running. This is the ordering an
    // operator uses — open the map, then start agents — and the one that makes
    // "sessions born during this watch" the interesting set.
    let mut tailer = fleet.watch();
    let mut world = fleet.world();

    let a = fleet.session("aaaaaaaa-0000-0000-0000-00000000000a");
    let b = fleet.session("bbbbbbbb-0000-0000-0000-00000000000b");
    let c = fleet.session("cccccccc-0000-0000-0000-00000000000c");

    // Three main agents, each with its own subtree, all in one checkout.
    a.main_record(read(&fleet.file("src/auth/token.rs"), "toolu_a0"));
    b.main_record(read(&fleet.file("src/render/frame.rs"), "toolu_b0"));
    c.main_record(read(&fleet.file("docs/README.md"), "toolu_c0"));
    a.worker_record("w-a1", edit(&fleet.file("src/auth/token.rs"), "toolu_a1"));
    a.worker_record("w-a2", edit(&fleet.file("src/auth/session.rs"), "toolu_a2"));
    b.worker_record("w-b1", edit(&fleet.file("src/render/frame.rs"), "toolu_b1"));
    c.main_record(read(&fleet.file("docs/README.md"), "toolu_c1"));

    let applied = drain(&mut tailer, &mut world, 7);
    assert!(
        applied >= 7,
        "only {applied} of 7 records reached the world"
    );

    assert_eq!(
        world.threads.len(),
        3,
        "three sessions in one repository are three threads, not one and not none: {:?}",
        world.threads.keys().collect::<Vec<_>>()
    );
    for (id, workers) in [(&a, 2usize), (&b, 1), (&c, 0)] {
        let thread = world
            .thread(&ThreadId::of_session(SessionId::new(&id.id)))
            .unwrap_or_else(|| panic!("session {} has no thread", id.id));
        assert_eq!(
            thread.workers.len(),
            workers,
            "session {} should own {workers} worker(s)",
            id.id
        );
    }
    assert!(
        world.unattributed.is_empty(),
        "every worker here has a parent session in its own file path"
    );
}

#[test]
fn a_worker_is_attributed_to_its_own_thread_and_never_to_another() {
    // The failure this rules out is the one that silently ruins territory: a
    // worker folded into the wrong thread feeds that thread's territory with
    // evidence it did not earn (PRD §6).
    let fleet = Fleet::new();
    let mut tailer = fleet.watch();
    let mut world = fleet.world();

    let a = fleet.session("11111111-0000-0000-0000-000000000001");
    let b = fleet.session("22222222-0000-0000-0000-000000000002");
    a.main_record(read(&fleet.file("docs/README.md"), "toolu_a"));
    b.main_record(read(&fleet.file("docs/README.md"), "toolu_b"));
    a.worker_record(
        "shared-name-a",
        edit(&fleet.file("src/auth/token.rs"), "t1"),
    );
    b.worker_record(
        "shared-name-b",
        edit(&fleet.file("src/render/frame.rs"), "t2"),
    );

    drain(&mut tailer, &mut world, 4);

    let ta = world
        .thread(&ThreadId::of_session(SessionId::new(&a.id)))
        .expect("thread a");
    let tb = world
        .thread(&ThreadId::of_session(SessionId::new(&b.id)))
        .expect("thread b");
    assert!(ta.worker(&WorkerId::new("shared-name-a")).is_some());
    assert!(ta.worker(&WorkerId::new("shared-name-b")).is_none());
    assert!(tb.worker(&WorkerId::new("shared-name-b")).is_some());
    assert!(tb.worker(&WorkerId::new("shared-name-a")).is_none());

    // And the attribution is recorded as evidence, not as a guess.
    let w = ta.worker(&WorkerId::new("shared-name-a")).expect("worker");
    assert_eq!(
        w.attribution,
        WorkerAttribution::RecordAgentId,
        "a record carrying `agentId` is the strongest link there is"
    );
    assert!(w.attribution.is_attributed());
}

#[test]
fn a_worker_with_no_reachable_parent_is_modelled_as_unattributed_and_adopted_when_it_can_be() {
    // ADR-0013 route 3, which is the *only* parent link 503 of 643 subagents
    // have: a workflow journal names a worker and a run, and nothing else. Until
    // the parent `Workflow` call is seen the worker has no thread, and PRD §5's
    // rule is explicit — "if a worker cannot be attributed to a thread, model
    // that explicitly rather than guessing". Folding it into whichever thread
    // happens to exist is how one territory absorbs another's evidence.
    let mut world = city_world();
    let t0 = Instant::now();

    // A thread is already running. It must NOT collect this worker.
    world.apply(&edit_event("a", "src/auth/token.rs", t0));

    world.apply(&journal_event(
        "wf-run-1",
        "orphan-1",
        t0 + Duration::from_secs(1),
    ));

    assert!(
        world
            .threads
            .values()
            .all(|t| t.worker(&WorkerId::new("orphan-1")).is_none()),
        "an unplaceable worker must not be adopted by a thread that merely exists"
    );
    assert_eq!(
        world.unattributed.len(),
        1,
        "it is modelled explicitly instead"
    );
    let parked = world.unattributed.values().next().expect("the orphan");
    assert_eq!(parked.id, WorkerId::new("orphan-1"));
    assert_eq!(parked.workflow_run.as_deref(), Some("wf-run-1"));
    assert!(
        !parked.reason.is_empty(),
        "the rail has to be able to say *why* it is unplaced"
    );
    assert_eq!(world.health.unattributed_workers, 1);

    // Now the parent `Workflow` call's result arrives, carrying the run id.
    // The link becomes knowable, so the worker is adopted rather than left
    // permanently orphaned by the order the files happened to be read in.
    world.apply(&workflow_result(
        "a",
        "wf-run-1",
        t0 + Duration::from_secs(2),
    ));
    let a = world
        .thread(&ThreadId::of_session(SessionId::new("a")))
        .expect("thread a");
    let adopted = a
        .worker(&WorkerId::new("orphan-1"))
        .expect("the parent call places it");
    assert_eq!(adopted.attribution, WorkerAttribution::WorkflowRun);
    assert!(world.unattributed.is_empty());
    assert_eq!(world.health.unattributed_workers, 0);
}

#[test]
fn an_unattributable_worker_does_not_stay_for_ever() {
    let mut world = city_world();
    let t0 = Instant::now();
    world.apply(&journal_event("wf-run-2", "orphan-2", t0));
    assert_eq!(world.unattributed.len(), 1);

    world.tick(t0 + UNATTRIBUTED_TTL + Duration::from_secs(1));
    assert!(
        world.unattributed.is_empty(),
        "a worker last seen an hour ago is not what is happening now"
    );
    assert_eq!(world.health.unattributed_workers, 0);
}

/// A `journal.jsonl` `started` line — a worker named by a run, and nothing else.
fn journal_event(run: &str, worker: &str, at: Instant) -> Event {
    let mut meta = EventMeta::now(Channel::Transcript);
    meta.observed = at;
    let body = serde_json::json!({ "type": "started", "agentId": worker });
    Event::new(
        meta,
        Payload::Transcript(Box::new(TranscriptEvent {
            kind: TranscriptRecordKind::Started,
            source: TranscriptSource::WorkflowJournal {
                run: run.to_owned(),
            },
            byte_offset: 0,
            record: body,
        })),
    )
}

/// The parent `Workflow` call's result, which is where the run id becomes
/// attached to a thread (ADR-0013 route 3).
fn workflow_result(session: &str, run: &str, at: Instant) -> Event {
    let mut meta = EventMeta::now(Channel::Transcript).with_session(SessionId::new(session));
    meta.observed = at;
    let body = serde_json::json!({
        "type": "user",
        "sessionId": session,
        "cwd": REPO,
        "message": {
            "role": "user",
            "content": [{ "type": "tool_result", "tool_use_id": "toolu_wf", "content": "ok" }],
        },
        // The two sources spell the run id differently: `toolUseResult.runId`
        // carries the `wf_` prefix, the directory name does not.
        "toolUseResult": { "runId": format!("wf_{run}") },
    });
    Event::new(
        meta,
        Payload::Transcript(Box::new(TranscriptEvent {
            kind: TranscriptRecordKind::User,
            source: TranscriptSource::Main,
            byte_offset: 0,
            record: body,
        })),
    )
}

// ---------------------------------------------------------------------------
// 2. Territory, per thread, from live observations
// ---------------------------------------------------------------------------

const REPO: &str = "C:/repo";

fn city_world() -> World {
    let mut tree = RepoTree {
        root: PathBuf::from(REPO),
        ..RepoTree::default()
    };
    for path in [
        "src/auth/token.rs",
        "src/auth/session.rs",
        "src/auth/login.rs",
        "src/auth/verify.rs",
        "src/render/frame.rs",
        "src/render/camera.rs",
        "src/render/glyph.rs",
        "docs/README.md",
        "docs/guide.md",
    ] {
        let logical = LogicalPath::new(path).expect("test path");
        tree.files
            .insert(logical.clone(), FileMeta::untracked(logical, 2_000));
    }
    let layout = polis_layout::city::generate(&tree);
    World::new(tree, layout)
}

/// An `Edit` of one repo-relative path by one session, observed at `at`.
fn edit_event(session: &str, rel: &str, at: Instant) -> Event {
    let mut meta = EventMeta::now(Channel::Transcript).with_session(SessionId::new(session));
    meta.observed = at;
    let body = serde_json::json!({
        "type": "assistant",
        "sessionId": session,
        "cwd": REPO,
        "gitBranch": "main",
        "message": {
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": format!("toolu_{rel}"),
                "name": "Edit",
                "input": { "file_path": format!("{REPO}/{rel}"), "old_string": "a", "new_string": "b" },
            }],
        },
    });
    Event::new(
        meta,
        Payload::Transcript(Box::new(TranscriptEvent {
            kind: TranscriptRecordKind::Assistant,
            source: TranscriptSource::Main,
            byte_offset: 0,
            record: body,
        })),
    )
}

#[test]
fn each_thread_converges_on_its_own_territory_and_an_undecided_thread_has_no_cloud() {
    // PRD §6.2: "Until then the thread renders with no cloud — an unplaced
    // marker in the status rail." That is asserted here as a *property of the
    // world*, so no renderer can invent a cloud the evidence does not support.
    let mut world = city_world();
    let t0 = Instant::now();

    for (i, rel) in [
        "src/auth/token.rs",
        "src/auth/session.rs",
        "src/auth/login.rs",
    ]
    .iter()
    .enumerate()
    {
        world.apply(&edit_event(
            "a",
            rel,
            t0 + Duration::from_secs(u64::try_from(i).unwrap()),
        ));
    }
    // Thread `b` has looked at two unrelated corners of the repository. Its
    // lowest common ancestor is the root, which is depth 0.
    world.apply(&edit_event("b", "src/render/frame.rs", t0));
    world.apply(&edit_event(
        "b",
        "docs/README.md",
        t0 + Duration::from_secs(1),
    ));
    world.tick(t0 + Duration::from_secs(4));

    let a = world
        .thread(&ThreadId::of_session(SessionId::new("a")))
        .unwrap();
    assert_eq!(
        a.territory.claim.as_ref().map(LogicalPath::as_str),
        Some("src/auth"),
        "three edits that agree are a territory"
    );
    assert!(
        a.territory.has_converged() && !a.territory.kernels.is_empty(),
        "and a converged territory has a field to draw"
    );

    let b = world
        .thread(&ThreadId::of_session(SessionId::new("b")))
        .unwrap();
    assert!(
        b.territory.claim.is_none(),
        "observations that do not agree are not a territory: {:?}",
        b.territory.convergence()
    );
    assert!(
        b.territory.convergence().depth < 2,
        "and the reason is answerable — depth {}, mass {:.2}",
        b.territory.convergence().depth,
        b.territory.convergence().mass_ratio
    );
}

#[test]
fn convergence_timing_is_measured_and_is_not_a_fixed_window() {
    // PRD §16 asks for exactly this number: "Assert convergence timing (how many
    // observations until emit)". PRD §6.2 refuses to make it a constant — "do
    // not emit a territory after a fixed number of reads" — so what is pinned is
    // that agreement converges quickly and disagreement never does.
    let mut world = city_world();
    let t0 = Instant::now();
    let mut converged_at = None;
    for (i, rel) in [
        "src/auth/token.rs",
        "src/auth/session.rs",
        "src/auth/login.rs",
        "src/auth/verify.rs",
    ]
    .iter()
    .enumerate()
    {
        world.apply(&edit_event(
            "a",
            rel,
            t0 + Duration::from_secs(u64::try_from(i).unwrap()),
        ));
        let t = world
            .thread(&ThreadId::of_session(SessionId::new("a")))
            .unwrap();
        if converged_at.is_none() && t.territory.has_converged() {
            converged_at = Some(i + 1);
        }
    }
    let n = converged_at.expect("agreeing observations must converge");
    eprintln!("territory converged after {n} agreeing observations");
    assert!(
        (2..=4).contains(&n),
        "convergence took {n} observations, which is neither 'when they agree' nor useful"
    );
}

#[test]
fn hysteresis_is_calm_expand_readily_contract_slowly_move_only_on_sustained_evidence() {
    // PRD §6.3, all three clauses, in the order the PRD states them. This is
    // "the single most important knob for whether this feels calm or twitchy".
    let mut world = city_world();
    let t0 = Instant::now();
    let id = ThreadId::of_session(SessionId::new("a"));

    for (i, rel) in [
        "src/auth/token.rs",
        "src/auth/session.rs",
        "src/auth/login.rs",
    ]
    .iter()
    .enumerate()
    {
        world.apply(&edit_event(
            "a",
            rel,
            t0 + Duration::from_millis(u64::try_from(i).unwrap() * 100),
        ));
    }
    assert_eq!(
        world
            .thread(&id)
            .unwrap()
            .territory
            .claim
            .as_ref()
            .map(LogicalPath::as_str),
        Some("src/auth")
    );

    // (1) Move only on sustained evidence: N=8 consecutive observations outside
    // the claim. Seven do nothing at all.
    let mut at = t0 + Duration::from_secs(1);
    for _ in 0..(DRIFT_CONFIRMATIONS - 1) {
        at += Duration::from_millis(100);
        world.apply(&edit_event("a", "src/render/frame.rs", at));
        assert_eq!(
            world
                .thread(&id)
                .unwrap()
                .territory
                .claim
                .as_ref()
                .map(LogicalPath::as_str),
            Some("src/auth"),
            "the district moved on {} observations; PRD §6.3 says {DRIFT_CONFIRMATIONS}",
            world.thread(&id).unwrap().territory.outside_streak
        );
    }
    // The eighth is allowed to move it — and only once the *evidence* also
    // agrees, which is the second gate and the reason one stray read cannot.
    for _ in 0..8 {
        at += Duration::from_millis(100);
        world.apply(&edit_event("a", "src/render/camera.rs", at));
    }
    assert_eq!(
        world
            .thread(&id)
            .unwrap()
            .territory
            .claim
            .as_ref()
            .map(LogicalPath::as_str),
        Some("src/render"),
        "sustained work elsewhere does eventually move the claim"
    );

    // (2) Expand readily: work in a parent of the claim takes effect at once,
    // because widening is not moving.
    let before = world.thread(&id).unwrap().territory.outside_streak;
    at += Duration::from_millis(100);
    world.apply(&edit_event("a", "src/render/glyph.rs", at));
    assert_eq!(
        world.thread(&id).unwrap().territory.outside_streak,
        0,
        "an observation inside the claim resets the streak (was {before})"
    );

    // (3) Contract slowly: a 90 s half-life, and nothing removed abruptly.
    let mass_before = world.thread(&id).unwrap().territory.mass();
    world.tick(at + DECAY_HALF_LIFE);
    let after = world.thread(&id).unwrap().territory.mass();
    let ratio = after / mass_before;
    assert!(
        (0.4..0.6).contains(&ratio),
        "one half-life must halve the field, not clear it: {mass_before:.2} -> {after:.2}"
    );
    assert!(
        world.thread(&id).unwrap().territory.claim.is_some(),
        "and the claim survives the decay — nothing is removed abruptly"
    );
}

// ---------------------------------------------------------------------------
// 3. Contention between two workers of one thread
// ---------------------------------------------------------------------------

/// An `Edit` by one worker of one session.
fn worker_edit(session: &str, worker: &str, rel: &str, at: Instant) -> Event {
    let mut meta = EventMeta::now(Channel::Transcript).with_session(SessionId::new(session));
    meta.observed = at;
    meta.worker = Some(WorkerId::new(worker));
    let body = serde_json::json!({
        "type": "assistant",
        "sessionId": session,
        "agentId": worker,
        "isSidechain": true,
        "cwd": REPO,
        "gitBranch": "main",
        "message": {
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": format!("toolu_{worker}_{rel}"),
                "name": "Edit",
                "input": { "file_path": format!("{REPO}/{rel}"), "old_string": "a", "new_string": "b" },
            }],
        },
    });
    Event::new(
        meta,
        Payload::Transcript(Box::new(TranscriptEvent {
            kind: TranscriptRecordKind::Assistant,
            source: TranscriptSource::Subagent {
                agent: WorkerId::new(worker),
                workflow_run: None,
            },
            byte_offset: 0,
            record: body,
        })),
    )
}

#[test]
fn two_workers_of_one_thread_writing_one_file_is_contention() {
    // The defect this closes: claims were keyed by thread, a thread is a
    // session, and a session now fans out to dozens of workers — so the state
    // PRD §11.1 ranks *above everything else* could not fire in the arrangement
    // that produces it most often. Measured on the operator's corpus: 77 logical
    // paths written by more than one worker inside one 69-worker session, and
    // two subagents 4.7 s apart on `settings.css`.
    let mut world = city_world();
    let t0 = Instant::now();

    world.apply(&worker_edit("a", "w1", "src/auth/token.rs", t0));
    world.apply(&worker_edit(
        "a",
        "w2",
        "src/auth/token.rs",
        t0 + Duration::from_millis(4_700),
    ));
    world.tick(t0 + Duration::from_secs(5));

    let hits = world.claims.hits(t0 + Duration::from_secs(5));
    assert_eq!(hits.len(), 1, "two workers, one file, one hit");
    let hit = &hits[0];
    assert!(hit.is_within_thread(), "both claimants are one session");
    assert_eq!(
        hit.severity,
        polis_world::contention::Severity::High,
        "same session means same checkout and same branch, so it is at least `same file`"
    );
    let (a, b) = hit.actors();
    assert_ne!(a, b, "the two ends are different actors");
    assert_eq!(a.thread, b.thread, "of one thread");

    assert!(
        world
            .attention
            .iter()
            .any(|m| matches!(m.kind, polis_world::attention::AttentionKind::Contention(_))),
        "and it reaches the attention layer, which is what makes it visible"
    );
}

#[test]
fn one_worker_editing_one_file_repeatedly_is_not_contention() {
    // The other half of the rule, and the one that keeps the map calm: a second
    // claim from the *same* actor refreshes the first.
    let mut world = city_world();
    let t0 = Instant::now();
    for i in 0..5 {
        world.apply(&worker_edit(
            "a",
            "w1",
            "src/auth/token.rs",
            t0 + Duration::from_millis(i * 500),
        ));
    }
    world.tick(t0 + Duration::from_secs(3));
    assert!(
        world.claims.hits(t0 + Duration::from_secs(3)).is_empty(),
        "one agent editing one file five times is work, not a collision"
    );
}

#[test]
fn two_threads_still_collide_and_the_relation_names_two_threads() {
    let mut world = city_world();
    let t0 = Instant::now();
    world.apply(&edit_event("a", "src/auth/token.rs", t0));
    world.apply(&edit_event(
        "b",
        "src/auth/token.rs",
        t0 + Duration::from_secs(1),
    ));
    world.tick(t0 + Duration::from_secs(2));

    let hits = world.claims.hits(t0 + Duration::from_secs(2));
    assert_eq!(hits.len(), 1);
    assert!(!hits[0].is_within_thread());
    let (x, y) = hits[0].threads();
    assert_ne!(x, y, "PRD §11.2: a relation between two threads");
}

// ---------------------------------------------------------------------------
// 4. Thread lifecycle
// ---------------------------------------------------------------------------

#[test]
fn a_quiet_thread_is_retired_and_leaves_nothing_behind() {
    let mut world = city_world();
    let t0 = Instant::now();
    world.apply(&edit_event("a", "src/auth/token.rs", t0));
    world.apply(&edit_event("b", "src/render/frame.rs", t0));
    let a = ThreadId::of_session(SessionId::new("a"));

    world.tick(t0 + Duration::from_secs(1));
    assert_eq!(world.threads.len(), 2);
    assert!(world
        .file(&LogicalPath::new("src/auth/token.rs").unwrap())
        .is_some_and(|f| f.touched_by.iter().any(|t| t == &a)));

    // Both go quiet. Nothing on the attention layer points at either — no Stop
    // hook ever arrived, which is the zero-setup case: Channel B is off.
    world.tick(t0 + THREAD_RETIRE_AFTER + Duration::from_secs(1));

    assert!(
        world.threads.is_empty(),
        "a thread silent for {THREAD_RETIRE_AFTER:?} is history, not a leak"
    );
    assert_eq!(world.health.threads_retired, 2);
    assert!(
        world.claims.is_empty(),
        "a claim must not outlive its claimant"
    );
    assert!(
        world
            .file(&LogicalPath::new("src/auth/token.rs").unwrap())
            .is_some_and(|f| f.touched_by.is_empty()),
        "and nothing points at a thread the rail cannot show"
    );
    assert!(world.attention.is_empty());
}

#[test]
fn a_thread_the_operator_still_has_to_act_on_is_never_retired_by_the_clock() {
    // PRD §11.2: "needs decision" persists until resolved, and "done,
    // unverified" is really "needs review" and persists too. Both would be
    // deleted by a retirement rule that only looked at the clock.
    let mut world = city_world();
    let t0 = Instant::now();
    world.apply(&edit_event("a", "src/auth/token.rs", t0));
    // The agent asks the operator a question. `AskUserQuestion` is Channel D's
    // exact, prospective form of "needs decision" — the `tool_use` block *is*
    // the question — so this is the real path and not a hand-placed mark.
    world.apply(&asks_event("a", t0 + Duration::from_secs(1)));
    let a = ThreadId::of_session(SessionId::new("a"));
    assert_eq!(world.thread(&a).unwrap().status, ThreadStatus::Waiting);

    world.tick(t0 + THREAD_RETIRE_AFTER * 4);
    assert!(
        world.thread(&a).is_some(),
        "a thread with a live mark stays until the operator deals with it"
    );
    assert_eq!(world.health.threads_retired, 0);
}

#[test]
fn a_transcript_that_vanishes_mid_tail_neither_corrupts_the_world_nor_stops_the_channel() {
    let fleet = Fleet::new();
    let mut tailer = fleet.watch();
    let mut world = fleet.world();

    let a = fleet.session("dddddddd-0000-0000-0000-00000000000d");
    let b = fleet.session("eeeeeeee-0000-0000-0000-00000000000e");
    a.main_record(edit(&fleet.file("src/auth/token.rs"), "t1"));
    b.main_record(edit(&fleet.file("src/render/frame.rs"), "t2"));
    drain(&mut tailer, &mut world, 2);
    assert_eq!(world.threads.len(), 2);
    let before = world.health.events_applied;

    // The file is deleted underneath the tail — a `/clear`, a cleanup script, an
    // operator tidying up. The session that is still alive must keep flowing.
    std::fs::remove_file(&a.main).expect("delete the transcript mid-tail");
    b.main_record(edit(&fleet.file("src/render/camera.rs"), "t3"));
    let more = drain(&mut tailer, &mut world, 1);

    assert!(more >= 1, "the surviving session's records still arrive");
    assert!(world.health.events_applied > before);
    assert!(
        world
            .thread(&ThreadId::of_session(SessionId::new(&b.id)))
            .is_some(),
        "the surviving thread is intact"
    );
    // The vanished session's thread is still there, because a deleted file is
    // not evidence that the agent stopped — it is only evidence that Polis can
    // no longer see it. Silence retires it, like every other ending.
    let roster = tailer.roster();
    assert!(
        !roster.sessions.iter().any(|s| s.session.as_str() == a.id),
        "a session with no transcript stops being reported rather than becoming a ghost"
    );
}

#[test]
fn the_thread_ceiling_binds_and_prefers_the_stalest() {
    let mut world = city_world();
    let t0 = Instant::now();
    for i in 0..(MAX_THREADS + 8) {
        world.apply(&edit_event(
            &format!("s{i:04}"),
            "src/auth/token.rs",
            t0 + Duration::from_millis(u64::try_from(i).unwrap()),
        ));
    }
    world.tick(t0 + Duration::from_secs(1));
    assert_eq!(
        world.threads.len(),
        MAX_THREADS,
        "the ceiling is a ceiling, not a suggestion"
    );
    assert_eq!(world.health.threads_evicted, 8);
    assert!(
        world
            .thread(&ThreadId::of_session(SessionId::new("s0000")))
            .is_none(),
        "the stalest goes first"
    );
    assert!(
        world
            .thread(&ThreadId::of_session(SessionId::new(format!(
                "s{:04}",
                MAX_THREADS + 7
            ))))
            .is_some(),
        "the newest stays"
    );
}

/// An `AskUserQuestion` call — Channel D's exact form of "needs decision".
fn asks_event(session: &str, at: Instant) -> Event {
    let mut meta = EventMeta::now(Channel::Transcript).with_session(SessionId::new(session));
    meta.observed = at;
    let body = serde_json::json!({
        "type": "assistant",
        "sessionId": session,
        "cwd": REPO,
        "message": {
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "toolu_ask",
                "name": "AskUserQuestion",
                "input": { "questions": [] },
            }],
        },
    });
    Event::new(
        meta,
        Payload::Transcript(Box::new(TranscriptEvent {
            kind: TranscriptRecordKind::Assistant,
            source: TranscriptSource::Main,
            byte_offset: 0,
            record: body,
        })),
    )
}

// ---------------------------------------------------------------------------
// 5. The synthetic load PRD §16 asks for
// ---------------------------------------------------------------------------

/// PRD §16's synthetic load, at its stated scale.
///
/// `#[ignore]`d for one reason and it is not the assertions: forty thousand
/// events cost twenty seconds of a saturated core in a debug build, and
/// `cargo test --workspace` runs crates' test binaries in parallel — which was
/// enough to push `polis-layout`'s wall-clock incremental-step budget over its
/// bound on a shared machine. A measurement that makes another measurement
/// flaky is not free.
///
/// ```text
/// cargo test -p polis-world --test live_threads -- --ignored --nocapture a_hundred
/// ```
#[test]
#[ignore = "20 s of full-core load; run it on demand, see this function's docs"]
fn a_hundred_threads_of_four_hundred_workers_stay_inside_the_budgets() {
    // > **Synthetic load.** An event generator that simulates 100 threads × 400
    // > subagents. Assert ingest budget and frame budget hold, and that
    // > dropped-event count stays at zero at 500/sec. (PRD §16)
    //
    // Run in a **debug** build, so every number here is pessimistic by roughly
    // an order of magnitude against the binary an operator runs.
    const THREADS: usize = 100;
    const WORKERS: usize = 400;

    let mut world = wide_city(THREADS);
    let t0 = Instant::now();
    let districts: Vec<String> = (0..THREADS).map(|d| format!("src/d{d:03}")).collect();

    let mut events = Vec::with_capacity(THREADS * WORKERS);
    for (t, district) in districts.iter().enumerate() {
        for w in 0..WORKERS {
            events.push(worker_edit(
                &format!("s{t:03}"),
                &format!("w{w:03}"),
                &format!("{district}/f{:02}.rs", w % 16),
                t0 + Duration::from_micros(u64::try_from(t * WORKERS + w).unwrap()),
            ));
        }
    }

    let began = Instant::now();
    for event in &events {
        world.apply(event);
    }
    let applied = began.elapsed();

    let tick_at = Instant::now();
    world.tick(t0 + Duration::from_secs(1));
    let ticked = tick_at.elapsed();

    let (publisher, reader) = polis_world::snapshot::from_world(&world);
    let publish_at = Instant::now();
    publisher.force(&world);
    let publish_cost = publish_at.elapsed();
    let frame = reader.load();

    #[allow(clippy::cast_precision_loss)] // printed rates, not decisions
    let per_event_us = applied.as_secs_f64() * 1e6 / events.len() as f64;
    #[allow(clippy::cast_precision_loss)]
    let rate = events.len() as f64 / applied.as_secs_f64();
    eprintln!(
        "{THREADS} threads x {WORKERS} workers = {} events: apply {applied:?} \
         ({per_event_us:.1} us/event, {rate:.0} events/sec), tick {ticked:?}, \
         publish {publish_cost:?}",
        events.len(),
    );
    eprintln!(
        "  snapshot: {} threads, {} workers, {} files, {} attention marks",
        frame.threads.len(),
        frame.threads.iter().map(|t| t.workers.len()).sum::<usize>(),
        frame.files.len(),
        frame.attention.len(),
    );

    assert_eq!(world.threads.len(), THREADS, "no thread was merged or lost");
    assert_eq!(
        world
            .threads
            .values()
            .map(|t| t.workers.len())
            .sum::<usize>(),
        THREADS * WORKERS,
        "every worker is attributed to exactly one thread"
    );
    assert!(world.unattributed.is_empty());
    assert_eq!(world.health.events_ignored, 0, "nothing was dropped");

    // PRD §13.1's ingest budget is 500 events/sec sustained. This is a debug
    // build, so the margin below is the pessimistic one.
    assert!(
        rate > 500.0,
        "ingest is {rate:.0} events/sec against PRD §13.1's 500/sec budget"
    );
    assert_eq!(world.health.dropped.values().sum::<u64>(), 0);

    // The frame budget is asserted at the scale that exists, immediately below.
    // At PRD §16's synthetic extreme it does **not** hold in a debug build, and
    // the reason is worth writing down rather than hiding behind a slack bound:
    // `WorldSnapshot` deep-clones every `Thread`, and at 100 x 400 that is
    // 40 000 workers copied per publish. The fix is a per-thread `Arc` keyed on a
    // generation counter, exactly as the publisher already does for the layout
    // and the file map; it is not free, because `Thread`'s fields are mutated
    // from a dozen places in `apply`.
    eprintln!(
        "  frame work at PRD §16's extreme: tick {ticked:?} + publish {publish_cost:?} = {:?}          (debug build; the 16.6 ms budget is asserted at real fleet scale below)",
        ticked + publish_cost
    );
}

#[test]
fn one_frame_of_world_work_fits_the_budget_at_the_scale_that_exists() {
    // The operator's largest measured session has 69 workers. Five of those at
    // once is a fleet bigger than anything on this machine has produced, and it
    // is the scale PRD §13.1's 16.6 ms frame has to hold at.
    const THREADS: usize = 5;
    const WORKERS: usize = 69;

    let mut world = wide_city(THREADS);
    let t0 = Instant::now();
    for t in 0..THREADS {
        for w in 0..WORKERS {
            world.apply(&worker_edit(
                &format!("s{t:03}"),
                &format!("w{w:03}"),
                &format!("src/d{t:03}/f{:02}.rs", w % 16),
                t0 + Duration::from_micros(u64::try_from(t * WORKERS + w).unwrap()),
            ));
        }
    }

    let (publisher, _reader) = polis_world::snapshot::from_world(&world);
    // Ten frames, so one unlucky scheduling slice cannot decide the result.
    let mut worst = Duration::ZERO;
    for i in 0..10 {
        let frame = Instant::now();
        world.tick(t0 + Duration::from_millis(u64::try_from(i).unwrap() * 16));
        publisher.force(&world);
        worst = worst.max(frame.elapsed());
    }
    eprintln!(
        "{THREADS} threads x {WORKERS} workers: worst frame of world work {worst:?} against          PRD §13.1's 16.6 ms"
    );
    assert!(
        worst < Duration::from_millis(16),
        "one frame's world work is {worst:?} against a 16.6 ms frame"
    );
}

/// A city wide enough that a hundred threads have a district each.
fn wide_city(districts: usize) -> World {
    let mut tree = RepoTree {
        root: PathBuf::from(REPO),
        ..RepoTree::default()
    };
    for d in 0..districts {
        for f in 0..16 {
            let path = format!("src/d{d:03}/f{f:02}.rs");
            let logical = LogicalPath::new(&path).expect("test path");
            tree.files
                .insert(logical.clone(), FileMeta::untracked(logical, 1_500));
        }
    }
    let layout = polis_layout::city::generate(&tree);
    World::new(tree, layout)
}

// ---------------------------------------------------------------------------
// 6. Territory overlap — contention's early warning (PRD §11.3)
// ---------------------------------------------------------------------------

#[test]
fn two_orchestrators_in_one_district_are_an_overlap_and_it_is_quieter_than_a_collision() {
    // > Two clouds overlapping means two orchestrators are claiming the same
    // > district, and it fires *before* anyone collides — while redirecting one
    // > is still cheap. Surface it as a distinct, quieter signal than
    // > file-level contention. (PRD §11.3)
    let mut world = city_world();
    let t0 = Instant::now();

    // Two sessions working the same district on *different* files, so nothing
    // has collided and nothing is being destroyed. This is the state the early
    // warning exists for.
    for (i, file) in ["token.rs", "session.rs", "login.rs", "verify.rs"]
        .into_iter()
        .enumerate()
    {
        let at = t0 + Duration::from_millis(u64::try_from(i).unwrap() * 100);
        world.apply(&edit_event("a", &format!("src/auth/{file}"), at));
    }
    for (i, file) in ["verify.rs", "login.rs", "session.rs", "token.rs"]
        .into_iter()
        .enumerate()
    {
        let at =
            t0 + Duration::from_secs(60) + Duration::from_millis(u64::try_from(i).unwrap() * 100);
        world.apply(&edit_event("b", &format!("src/auth/{file}"), at));
    }
    world.tick(t0 + Duration::from_secs(61));

    assert!(
        world
            .attention
            .iter()
            .all(|m| !matches!(m.kind, polis_world::attention::AttentionKind::Contention(_))),
        "the two sessions never wrote one file inside the TTL, so nothing is \
         being destroyed and the loud signal must stay silent"
    );
    assert_eq!(
        world.overlaps.len(),
        1,
        "but they are in each other's district, and that is worth knowing while \
         redirecting one is still cheap: {:?}",
        world.overlaps
    );
    let overlap = &world.overlaps[0];
    assert_ne!(overlap.a, overlap.b);
    assert!(
        overlap.score > 0.0 && overlap.score <= 1.0,
        "{}",
        overlap.score
    );
    assert!(
        overlap.same_district(),
        "both converged on src/auth: {} vs {}",
        overlap.claim_a,
        overlap.claim_b
    );

    // A quiet signal that restarted its animation every tick would not be
    // quiet, so the pair keeps the instant it was first seen.
    let since = overlap.since;
    world.tick(t0 + Duration::from_secs(62));
    assert_eq!(world.overlaps[0].since, since, "carried forward, not reset");

    // And it is not on the attention layer, where it would compete for the eye
    // with the state that means work is actively being destroyed.
    assert!(
        world.attention.is_empty(),
        "an overlap is not an attention mark: {:?}",
        world.attention
    );
}

#[test]
fn distant_districts_do_not_overlap_and_an_unconverged_thread_has_no_cloud_to_overlap() {
    let mut world = city_world();
    let t0 = Instant::now();
    for (i, file) in ["token.rs", "session.rs", "login.rs", "verify.rs"]
        .into_iter()
        .enumerate()
    {
        let at = t0 + Duration::from_millis(u64::try_from(i).unwrap() * 100);
        world.apply(&edit_event("a", &format!("src/auth/{file}"), at));
    }
    for (i, file) in ["frame.rs", "camera.rs", "glyph.rs"]
        .into_iter()
        .enumerate()
    {
        let at = t0 + Duration::from_millis(u64::try_from(i).unwrap() * 100);
        world.apply(&edit_event("b", &format!("src/render/{file}"), at));
    }
    // A third thread with a single observation: no convergence, so no cloud.
    world.apply(&edit_event("c", "docs/README.md", t0));
    world.tick(t0 + Duration::from_secs(1));

    assert!(
        world.overlaps.is_empty(),
        "two agents in two districts are not a warning about anything: {:?}",
        world.overlaps
    );
    assert!(
        world
            .thread(&ThreadId::of_session(SessionId::new("c")))
            .is_some_and(|t| t.territory.claim.is_none()),
        "and an unconverged territory has no cloud to overlap (PRD §6.2)"
    );
}

#[test]
fn the_early_warning_and_the_claim_table_hold_the_frame_at_prd_16s_load() {
    // Two things this milestone made quadratic, measured at the load PRD §16
    // specifies rather than at the load the operator's machine has produced:
    //
    // * **Overlaps** pair every converged territory with every other, so a
    //   hundred threads is 4 950 pairs, each a kernel-by-kernel field product.
    // * **Claims** now survive their write landing for the rest of the 30 s TTL,
    //   so the table holds every path a fleet has touched inside that window
    //   instead of emptying itself a second after each edit — and
    //   `ClaimTable::hits` walks all of them every tick.
    //
    // Both are inside `World::tick`, which PRD §13.1 budgets at one 16.6 ms
    // frame alongside everything else it does.
    const THREADS: usize = 100;
    const FILES: usize = 16;

    let mut world = wide_city(THREADS);
    let t0 = Instant::now();
    // Every thread converges on its own district and writes every file in it,
    // so the claim table is at its widest and every territory has a cloud.
    for t in 0..THREADS {
        for f in 0..FILES {
            world.apply(&edit_event(
                &format!("s{t:03}"),
                &format!("src/d{t:03}/f{f:02}.rs"),
                t0 + Duration::from_micros(u64::try_from(t * FILES + f).unwrap()),
            ));
        }
    }
    // Plus a shared file every thread writes, which is the worst case for the
    // pairing inside one path.
    for t in 0..THREADS {
        world.apply(&edit_event(
            &format!("s{t:03}"),
            "src/d000/f00.rs",
            t0 + Duration::from_millis(1 + u64::try_from(t).unwrap()),
        ));
    }
    world.tick(t0 + Duration::from_millis(200));
    let converged = world
        .threads
        .values()
        .filter(|t| t.territory.claim.is_some())
        .count();
    assert!(
        converged >= THREADS / 2,
        "the load has to actually produce clouds to pair: {converged}"
    );

    let mut worst = Duration::ZERO;
    for i in 0..10 {
        let frame = Instant::now();
        world.tick(t0 + Duration::from_millis(200 + u64::try_from(i).unwrap() * 16));
        worst = worst.max(frame.elapsed());
    }
    eprintln!(
        "{THREADS} threads, {converged} clouds ({} pairs), {} claimed paths, \
         {} overlaps: worst tick {worst:?} against PRD §13.1's 16.6 ms",
        converged * converged.saturating_sub(1) / 2,
        world.claims.len(),
        world.overlaps.len()
    );
    assert!(
        world.overlaps.len() <= polis_world::contention::MAX_OVERLAPS,
        "the quiet signal is capped too: {}",
        world.overlaps.len()
    );
    assert!(
        worst < Duration::from_millis(16),
        "one tick is {worst:?} against a 16.6 ms frame"
    );
}
