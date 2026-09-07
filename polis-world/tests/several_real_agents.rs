//! The M4 acceptance test: **several real agents at once**, in one repository,
//! with zero configuration (PRD §15 M4).
//!
//! M3's acceptance test (`polis-ingest/tests/live_discovery.rs`) answered *"is
//! one agent discovered"*. This one answers the operator's sentence in the
//! number it was actually said in:
//!
//! > *"i want to see all agents active in a repository on the machine"*
//!
//! It starts a watch on a checkout **before** anything is running in it, lets
//! two or three real `claude` processes work in that checkout, drives one
//! [`World`] from what the tailer reads, and then reports the four numbers this
//! milestone is judged on:
//!
//! 1. **Thread count** — one per session, never merged, never invented.
//! 2. **Worker attribution rate** — attributed workers over workers seen, with
//!    the route each one was attributed by. An unattributed worker is *reported*,
//!    not hidden and not guessed (PRD §5).
//! 3. **Territory convergence timing** — how many observations a thread made
//!    before PRD §6.2's two gates agreed and it got a cloud, per thread.
//! 4. **Stability** — how many times a converged claim changed district, which
//!    PRD §6.3 says may only happen on `N=8` sustained observations outside it.
//!
//! It is `#[ignore]`d: it needs `claude` on `PATH`, a network round trip, and an
//! operator's API quota.
//!
//! ```text
//! # Watch a scratch checkout and start three agents in it, in one command.
//! set POLIS_ACCEPTANCE_REPO=C:\path\to\scratch-repo
//! set POLIS_ACCEPTANCE_SPAWN=3
//! cargo test -p polis-world --test several_real_agents -- --ignored --nocapture
//! ```
//!
//! Without `POLIS_ACCEPTANCE_SPAWN` it starts nothing and waits for the operator
//! to start agents by hand in their own terminals — which is the stricter test,
//! because nothing about those processes knows Polis exists.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use polis_events::{Event, LogicalPath, PathMapper, ThreadId};
use polis_ingest::live::{LiveTailer, Scope};
use polis_world::{WorkerAttribution, World};

/// How long the watch runs before it reports what it saw.
const DEADLINE: Duration = Duration::from_secs(240);

/// How often the tailer is polled. Matches what the window uses.
const POLL: Duration = Duration::from_millis(200);

/// How often the roster is rebuilt.
const RESCAN: Duration = Duration::from_secs(2);

/// What one thread's territory did over the run.
#[derive(Debug, Default)]
struct Track {
    /// Observations recorded when the claim first appeared.
    converged_after: Option<u64>,
    /// Wall time from the thread's first event to its first claim.
    converged_in: Option<Duration>,
    /// Every district the claim has held, in order.
    claims: Vec<LogicalPath>,
    /// Observations now.
    observations: u64,
    /// First time this thread was seen.
    first_seen: Option<Instant>,
}

#[test]
#[ignore = "needs several real Claude Code sessions; see this file's module docs"]
fn several_real_agents_in_one_repository_are_several_threads() {
    let Some(repo) = repo_under_test() else {
        eprintln!("skipped: set POLIS_ACCEPTANCE_REPO to a git checkout to run this");
        return;
    };
    let projects = polis_ingest::default_claude_projects_dir()
        .expect("this machine must have a home directory");
    assert!(
        projects.is_dir(),
        "no {} — run Claude Code once first",
        projects.display()
    );

    // The city first, so the live layer has somewhere to land. Territory
    // convergence is path evidence and would work without it; the kernels — the
    // thing a cloud is drawn from — would not.
    let index = polis_repo::tree::RepoIndex::open(&repo).expect("index the checkout");
    let layout = polis_layout::city::generate(index.tree());
    eprintln!(
        "city: {} buildings, {} districts, extent {:.0}",
        layout.buildings.len(),
        layout.districts.len(),
        layout.extent
    );
    let mut world = World::new(index.tree().clone(), layout);

    // The watch starts BEFORE anything runs in the checkout.
    let mapper = PathMapper::new(&repo).expect("the repository path must be mappable");
    let mut tailer = LiveTailer::open(&projects, Scope::Repo(mapper));
    let start = tailer.roster();
    eprintln!("watching {} — {}", repo.display(), start.headline());
    assert_eq!(
        start.here().filter(|s| s.activity.is_live()).count(),
        0,
        "run this against a checkout with no agent already in it: {}",
        start.headline()
    );

    let mut agents = spawn_agents(&repo);

    let began = Instant::now();
    let mut last_rescan = Instant::now();
    let mut events: Vec<Event> = Vec::new();
    let mut total = 0usize;
    let mut tracks: BTreeMap<ThreadId, Track> = BTreeMap::new();
    let mut peak_threads = 0usize;

    while began.elapsed() < DEADLINE {
        if last_rescan.elapsed() >= RESCAN {
            tailer.rescan();
            last_rescan = Instant::now();
        }
        events.clear();
        tailer.poll(&mut events);
        for event in &events {
            world.apply(event);
        }
        total += events.len();
        let now = Instant::now();
        world.tick(now);

        // Sample the territory of every thread every pass, because convergence
        // timing is a question about *when*, and a sample taken at the end
        // cannot answer it.
        for thread in world.threads.values() {
            let track = tracks.entry(thread.id.clone()).or_default();
            track.first_seen.get_or_insert(now);
            track.observations = thread.territory.observations;
            if let Some(claim) = &thread.territory.claim {
                if track.claims.last() != Some(claim) {
                    track.claims.push(claim.clone());
                }
                if track.converged_after.is_none() {
                    track.converged_after = Some(thread.territory.observations);
                    track.converged_in = track.first_seen.map(|t| now.saturating_duration_since(t));
                }
            }
        }
        peak_threads = peak_threads.max(world.threads.len());

        // Every spawned agent has exited and the map has stopped changing.
        if !agents.is_empty()
            && agents
                .iter_mut()
                .all(|c| matches!(c.try_wait(), Ok(Some(_))))
            && events.is_empty()
            && total > 0
        {
            // One more quiet second, so the last records are read.
            std::thread::sleep(Duration::from_secs(1));
            tailer.poll(&mut events);
            for event in &events {
                world.apply(event);
            }
            total += events.len();
            world.tick(Instant::now());
            break;
        }
        std::thread::sleep(POLL);
    }
    for child in &mut agents {
        let _ = child.wait();
    }

    report(
        &world,
        &tracks,
        &tailer,
        total,
        began.elapsed(),
        peak_threads,
    );
    check(&world, &tracks, total, began.elapsed());
}

/// Everything the milestone claims, once the run is over.
fn check(world: &World, tracks: &BTreeMap<ThreadId, Track>, total: usize, elapsed: Duration) {
    assert!(total > 0, "no events reached the world at all");
    assert!(
        world.threads.len() >= 2,
        "M4 is about the plural case: {} thread(s) seen. Start at least two agents.",
        world.threads.len()
    );
    for thread in world.threads.values() {
        assert_eq!(
            thread.id,
            ThreadId::of_session(thread.session_id.clone()),
            "a thread is exactly one session's subtree"
        );
        // PRD §6.2: a thread with no converged territory has no cloud. Whatever
        // the run produced, that invariant must hold for every thread.
        if thread.territory.claim.is_none() {
            assert!(
                !thread.territory.has_converged(),
                "a thread with no claim must not report a converged territory"
            );
        }
    }
    // PRD §6.3: no thread may have wandered between districts. Two claims are
    // fine when the second contains or is contained by the first — that is
    // widening, not moving.
    for (id, track) in tracks {
        let moves = track
            .claims
            .windows(2)
            .filter(|w| !w[0].starts_with(&w[1]) && !w[1].starts_with(&w[0]))
            .count();
        assert!(
            moves <= 1,
            "{id:?} changed district {moves} times in {elapsed:?}; PRD §6.3 calls that twitchy"
        );
    }
}

/// Prints everything the run measured.
fn report(
    world: &World,
    tracks: &BTreeMap<ThreadId, Track>,
    tailer: &LiveTailer,
    events: usize,
    elapsed: Duration,
    peak_threads: usize,
) {
    let roster = tailer.roster();
    eprintln!("\n--- M4 measurement -------------------------------------------");
    eprintln!("elapsed              {elapsed:?}");
    eprintln!("events applied       {events}");
    eprintln!("roster               {}", roster.headline());
    eprintln!(
        "threads              {} now, {peak_threads} at peak",
        world.threads.len()
    );

    let mut workers = 0usize;
    let mut by_route: BTreeMap<&'static str, usize> = BTreeMap::new();
    for thread in world.threads.values() {
        for w in &thread.workers {
            workers += 1;
            *by_route.entry(worker_route(&w.attribution)).or_default() += 1;
        }
    }
    let unattributed = world.unattributed.len();
    let seen = workers + unattributed;
    if seen == 0 {
        eprintln!("workers              none (no subagent was spawned in this run)");
    } else {
        #[allow(clippy::cast_precision_loss)] // a percentage, for one printed line
        let rate = workers as f64 / seen as f64 * 100.0;
        eprintln!("workers              {workers}/{seen} attributed ({rate:.1}%)");
        for (route, n) in &by_route {
            eprintln!("  via {route:<20} {n}");
        }
        if unattributed > 0 {
            eprintln!("  unattributed       {unattributed} (reported, never guessed)");
        }
    }

    eprintln!("\nper thread:");
    for thread in world.threads_for_rail() {
        let track = tracks.get(&thread.id);
        let claim = thread.territory.claim.as_ref().map_or_else(
            || "(no cloud — unplaced in the rail)".to_owned(),
            |c| format!("{} ", c.as_str()),
        );
        let convergence = thread.territory.convergence();
        eprintln!(
            "  {:<20} {:<7} obs {:<5} claim {claim:<28} depth {} mass {:.2}",
            short(&thread.id),
            thread.status.to_string(),
            thread.territory.observations,
            convergence.depth,
            convergence.mass_ratio,
        );
        if let Some(track) = track {
            match (track.converged_after, track.converged_in) {
                (Some(n), Some(t)) => eprintln!(
                    "    converged after {n} observations ({t:?}); claims held: {:?}",
                    track
                        .claims
                        .iter()
                        .map(LogicalPath::as_str)
                        .collect::<Vec<_>>()
                ),
                _ => eprintln!("    never converged — no cloud, which is PRD §6.2 working"),
            }
        }
        eprintln!(
            "    workers {} ({} running), tool calls {}, failures {}",
            thread.workers.len(),
            thread.running_workers(),
            thread.tool_calls,
            thread.failures,
        );
    }
    let hits = world.claims.hits(world.now());
    eprintln!(
        "\ncontention           {} live hit(s), {} total since start",
        hits.len(),
        world.claims.total_hits()
    );
    for hit in hits.iter().take(4) {
        eprintln!(
            "  {:?} {} {}",
            hit.severity,
            hit.path().as_str(),
            if hit.is_within_thread() {
                "(two workers of one thread)"
            } else {
                "(two threads)"
            }
        );
    }
    eprintln!(
        "health               drift {}, unmapped {}, unplaced obs {}, retired {}",
        world.health.drift,
        world.health.unmapped_paths,
        world.health.unplaced_observations,
        world.health.threads_retired,
    );
    eprintln!("--------------------------------------------------------------\n");
}

/// The last twelve characters of a thread id — enough to tell sessions apart.
fn short(id: &ThreadId) -> String {
    let text = format!("{id:?}");
    let n = text.len();
    text[n.saturating_sub(14)..].to_owned()
}

/// The checkout to watch, from the environment.
fn repo_under_test() -> Option<PathBuf> {
    let raw = std::env::var_os("POLIS_ACCEPTANCE_REPO")?;
    let path = PathBuf::from(raw);
    path.canonicalize().ok().as_deref().map(strip_verbatim)
}

/// Windows' `\\?\` prefix, removed for a plain drive path.
fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\")
        .map_or_else(|| path.to_path_buf(), PathBuf::from)
}

/// Starts `POLIS_ACCEPTANCE_SPAWN` agents in the checkout, when asked to.
///
/// Each gets a different one-line prompt aimed at a different directory, because
/// three agents that all read the same file would converge on one territory and
/// prove nothing about the plural case. They are still one prompt each: this
/// costs an operator's API quota.
fn spawn_agents(repo: &Path) -> Vec<std::process::Child> {
    let Some(raw) = std::env::var_os("POLIS_ACCEPTANCE_SPAWN") else {
        eprintln!(
            "  now start two or three `claude` sessions in {} …",
            repo.display()
        );
        return Vec::new();
    };
    let n: usize = raw.to_string_lossy().parse().unwrap_or(1);
    let prompts = [
        "Read every file in src/auth and reply with the number of lines in each. Do not edit anything.",
        "Read every file in src/render and reply with the number of lines in each. Do not edit anything.",
        // One of them fans out, so the run measures worker attribution and not
        // only presence. A thread is a main agent *plus its worker subtree*.
        "Use the Task tool to launch one general-purpose subagent that reads every file in src/notes and reports their line counts. Reply with what it reports. Do not edit anything yourself.",
    ];
    let mut out = Vec::new();
    for (i, prompt) in prompts.iter().take(n).enumerate() {
        let child = std::process::Command::new("claude")
            .current_dir(repo)
            .args(["-p", prompt])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        match child {
            Ok(child) => {
                eprintln!("  spawned agent {} in {}", i + 1, repo.display());
                out.push(child);
            }
            Err(error) => eprintln!("  could not spawn claude ({error}); start one by hand"),
        }
        // Staggered, so the three sessions overlap rather than starting in
        // lockstep — which is what a real fleet looks like.
        std::thread::sleep(Duration::from_millis(700));
    }
    out
}

/// The route a worker was attributed by, in the words ADR-0013 uses.
fn worker_route(attribution: &WorkerAttribution) -> &'static str {
    match attribution {
        WorkerAttribution::RecordAgentId => "record agentId",
        WorkerAttribution::SpawnToolUse => "spawn toolUseId",
        WorkerAttribution::WorkflowRun => "workflow runId",
        WorkerAttribution::TranscriptFile => "transcript filename",
        WorkerAttribution::Unknown => "unattributed",
    }
}
