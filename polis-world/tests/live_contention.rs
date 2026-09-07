//! Two real agents, one file, one red link (PRD §11.3, §11.2c).
//!
//! Everything else about contention is measured against recorded sessions in
//! `contention_on_real_sessions.rs`. This is the other half, and it is the half
//! a replay **structurally cannot** do: it starts two real `claude` processes in
//! one checkout, points them both at one file, and waits for the claim table to
//! join them.
//!
//! It is `#[ignore]`d — it needs `claude` on `PATH`, a network round trip and an
//! operator's API quota:
//!
//! ```text
//! cargo test --release -p polis-world --test live_contention -- --ignored --nocapture
//! ```
//!
//! # What has to be true for this to fire, and why each part is load-bearing
//!
//! * **Two agents, two sessions, two threads.** Cross-session contention is the
//!   case a `ThreadId`-keyed table could already see; running it live is what
//!   proves the actor key did not break it.
//! * **One logical file.** Claims key on the logical path, so the two agents
//!   collide on one entry.
//! * **Inside 30 s of each other.** The TTL is the window, and the *landing* is
//!   when it starts — which is the fix this milestone turns on. Before it, a
//!   claim was deleted the moment the write landed, and two agents seconds apart
//!   held nothing in common.
//! * **Zero configuration.** No hooks, no `OTEL_*`, no wrapper: the transcript
//!   tailer finds both sessions under `~/.claude/projects` on its own, which is
//!   the operator's *"i want to see all agents active in a repository"*.
//!
//! The scratch checkout is created and removed by the test; the operator's own
//! repositories and their `~/.claude/settings.json` are never touched.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use polis_events::{Event, PathMapper};
use polis_ingest::live::{LiveTailer, Scope};
use polis_world::attention::AttentionKind;
use polis_world::contention::{Contention, Severity};
use polis_world::World;

/// How long the run waits for the two agents to collide.
const DEADLINE: Duration = Duration::from_secs(300);

/// Tailer poll interval. Matches what the window uses.
const POLL: Duration = Duration::from_millis(200);

/// How often the roster is rebuilt, so a session that starts after the watch is
/// still found.
const RESCAN: Duration = Duration::from_secs(2);

/// The one file both agents are pointed at.
const TARGET: &str = "src/shared.txt";

#[test]
#[ignore = "starts two real Claude Code sessions; see this file's module docs"]
// One run, reported in full. Splitting it would put the poll loop, the numbers
// it gathered and the assertions about them in three places, and this file's
// value is that a reader can see the whole experiment at once.
#[allow(clippy::too_many_lines)]
fn two_real_agents_editing_one_file_are_a_link_across_the_map() {
    let Some(projects) = polis_ingest::default_claude_projects_dir() else {
        eprintln!("skipped: no home directory");
        return;
    };
    if !projects.is_dir() {
        eprintln!(
            "skipped: no {} — run Claude Code once first",
            projects.display()
        );
        return;
    }
    let Some(repo) = scratch_repo() else {
        eprintln!("skipped: could not create a scratch checkout");
        return;
    };
    eprintln!("scratch checkout: {}", repo.display());

    let index = polis_repo::tree::RepoIndex::open(&repo).expect("index the scratch checkout");
    let layout = polis_layout::city::generate(index.tree());
    eprintln!(
        "city: {} buildings, {} districts",
        layout.buildings.len(),
        layout.districts.len()
    );
    let mut world = World::new(index.tree().clone(), layout);

    // The watch starts BEFORE anything runs in the checkout — the zero-setup
    // path, discovering sessions it was never told about.
    let mapper = PathMapper::new(&repo).expect("the checkout must be mappable");
    let mut tailer = LiveTailer::open(&projects, Scope::Repo(mapper));
    tailer.rescan();
    assert_eq!(
        tailer
            .roster()
            .here()
            .filter(|s| s.activity.is_live())
            .count(),
        0,
        "the scratch checkout must start empty"
    );

    let mut agents = spawn_pair(&repo);
    if agents.is_empty() {
        cleanup(&repo);
        eprintln!("skipped: could not start `claude`");
        return;
    }

    let began = Instant::now();
    let mut last_rescan = Instant::now();
    let mut events: Vec<Event> = Vec::new();
    let mut applied = 0usize;
    let mut first_hit: Option<(Duration, Contention)> = None;
    let mut sessions_seen: BTreeSet<String> = BTreeSet::new();
    let mut writers: BTreeSet<String> = BTreeSet::new();
    let mut quiet_since: Option<Instant> = None;

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
        applied += events.len();
        let now = Instant::now();
        world.tick(now);

        for thread in world.threads.values() {
            sessions_seen.insert(thread.session_id.as_str().to_owned());
        }
        for (path, claims) in world.claims.paths() {
            if path.as_str().ends_with("shared.txt") {
                for claim in claims {
                    writers.insert(format!("{}", claim.thread));
                }
            }
        }

        if first_hit.is_none() {
            if let Some(hit) = world.attention.iter().find_map(|m| match &m.kind {
                AttentionKind::Contention(c) => Some((**c).clone()),
                _ => None,
            }) {
                eprintln!(
                    "\n>>> contention at {:?}: {} — {:?} ({})",
                    began.elapsed(),
                    hit.path(),
                    hit.severity,
                    hit.severity.label()
                );
                first_hit = Some((began.elapsed(), hit));
            }
        }

        let all_done = agents
            .iter_mut()
            .all(|c| matches!(c.try_wait(), Ok(Some(_))));
        if all_done && events.is_empty() && applied > 0 {
            // The last records may still be being flushed; stop after a second
            // of genuine quiet, and never before a hit has had its chance.
            let since = *quiet_since.get_or_insert(now);
            if now.saturating_duration_since(since) > Duration::from_secs(2) {
                break;
            }
        } else {
            quiet_since = None;
        }
        std::thread::sleep(POLL);
    }
    for child in &mut agents {
        let _ = child.kill();
        let _ = child.wait();
    }

    eprintln!("\n--- live contention ------------------------------------------");
    eprintln!("elapsed          {:?}", began.elapsed());
    eprintln!("events applied   {applied}");
    eprintln!("threads          {}", world.threads.len());
    eprintln!("sessions seen    {sessions_seen:?}");
    eprintln!("claimed {TARGET} {writers:?}");
    eprintln!("claim paths      {}", world.claims.len());
    eprintln!("table hits       {}", world.claims.total_hits());

    let contents = std::fs::read_to_string(repo.join(TARGET)).unwrap_or_default();
    eprintln!("target file      {:?}", contents.trim());

    if let Some((at, hit)) = &first_hit {
        let (a, b) = hit.actors();
        let link = hit.link(&world.layout, |id| world.thread(id));
        eprintln!("FIRED after      {at:?}");
        eprintln!("  path           {}", hit.path());
        eprintln!(
            "  severity       {:?} ({})",
            hit.severity,
            hit.severity.label()
        );
        eprintln!("  precision      {:?}", hit.precision);
        eprintln!("  within thread  {}", hit.is_within_thread());
        eprintln!("  ends           {a:?}\n                 {b:?}");
        eprintln!(
            "  link           {:?} -> {:?} (degenerate: {})",
            link.at_a,
            link.at_b,
            link.is_degenerate()
        );
    } else {
        eprintln!("NO CONTENTION — the two agents never held the file at once");
    }
    cleanup(&repo);

    assert!(applied > 0, "no events reached the world at all");
    assert!(
        world.threads.len() >= 2,
        "two agents are two threads; saw {}",
        world.threads.len()
    );
    let (_, hit) = first_hit.expect(
        "two agents editing one file inside the 30 s TTL must produce PRD \
         §11.1's top-ranked state",
    );
    assert!(
        hit.path().as_str().ends_with("shared.txt"),
        "the collision is on the file they were both pointed at: {}",
        hit.path()
    );
    assert!(
        hit.severity >= Severity::Medium,
        "one file, two agents: {:?}",
        hit.severity
    );
    let (a, b) = hit.actors();
    assert_ne!(
        a, b,
        "PRD §11.2c: a relation between two, never a badge on one"
    );
}

/// A scratch git checkout with one file both agents will edit.
fn scratch_repo() -> Option<PathBuf> {
    let base = std::env::var_os("POLIS_SCRATCH").map_or_else(std::env::temp_dir, PathBuf::from);
    let repo = base.join(format!("polis-live-contention-{}", std::process::id()));
    std::fs::create_dir_all(repo.join("src")).ok()?;
    // Enough files that the city has geometry and the two agents have somewhere
    // to be other than on top of each other.
    for (name, body) in [
        ("src/shared.txt", "one\ntwo\nthree\n"),
        ("src/alpha.txt", "alpha\n"),
        ("src/beta.txt", "beta\n"),
        ("README.md", "scratch\n"),
    ] {
        std::fs::write(repo.join(name), body).ok()?;
    }
    // A project-local settings file, so nothing here reaches the operator's own
    // `~/.claude/settings.json`.
    std::fs::create_dir_all(repo.join(".claude")).ok()?;
    std::fs::write(
        repo.join(".claude/settings.json"),
        "{ \"permissions\": { \"allow\": [\"Edit\", \"Write\", \"Read\"] } }\n",
    )
    .ok()?;
    for args in [
        vec!["init", "-q"],
        vec!["add", "-A"],
        vec![
            "-c",
            "user.email=polis@test",
            "-c",
            "user.name=polis",
            "commit",
            "-qm",
            "seed",
        ],
    ] {
        let ok = Command::new("git")
            .current_dir(&repo)
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return None;
        }
    }
    Some(repo)
}

fn cleanup(repo: &Path) {
    let _ = std::fs::remove_dir_all(repo);
}

/// Two agents, started together, pointed at the same file.
///
/// Both are told to edit `src/shared.txt` **and nothing else**, so the collision
/// is the thing under test rather than a coincidence. They are started in the
/// same instant rather than staggered, because the window is what is being
/// measured.
fn spawn_pair(repo: &Path) -> Vec<std::process::Child> {
    let prompts = [
        "Append one line saying `alpha` to the end of src/shared.txt using the Edit tool. \
         Then stop. Do not read or touch any other file.",
        "Append one line saying `beta` to the end of src/shared.txt using the Edit tool. \
         Then stop. Do not read or touch any other file.",
    ];
    let mut out = Vec::new();
    for (i, prompt) in prompts.iter().enumerate() {
        match Command::new("claude")
            .current_dir(repo)
            .args(["-p", prompt, "--permission-mode", "acceptEdits"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => {
                eprintln!("  spawned agent {}", i + 1);
                out.push(child);
            }
            Err(error) => eprintln!("  could not spawn claude ({error})"),
        }
    }
    out
}
