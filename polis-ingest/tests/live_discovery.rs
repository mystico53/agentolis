//! The M3 acceptance test: start a watch, then start a **real** Claude Code
//! session, and confirm it is discovered (PRD §15 M3).
//!
//! Everything else about live discovery is unit-tested against synthetic
//! transcripts. This is the one test that answers the operator's actual
//! question — *"i want to see all agents active in a repository on the
//! machine"* — by watching a repository and then having a real agent work in it,
//! started by a separate process that knows nothing about Polis: no hooks, no
//! telemetry environment, no wrapper.
//!
//! It is `#[ignore]`d because it needs `claude` on `PATH`, a network round trip
//! and an operator's API quota. Run it by hand:
//!
//! ```text
//! # Terminal 1 — watch a scratch repository for 90 seconds.
//! set POLIS_ACCEPTANCE_REPO=C:\path\to\scratch-repo
//! cargo test -p polis-ingest --test live_discovery -- --ignored --nocapture
//!
//! # Terminal 2 — while that runs, start an agent in it. Nothing else.
//! cd C:\path\to\scratch-repo && claude -p "reply with the word ok"
//! ```
//!
//! With `POLIS_ACCEPTANCE_SPAWN=1` it starts the agent itself, so the whole
//! thing is one command.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use polis_events::{Event, PathMapper, Payload};
use polis_ingest::live::{Fit, LiveTailer, Scope};

/// How long the watch runs before giving up on seeing an agent.
const DEADLINE: Duration = Duration::from_secs(120);

/// The poll interval, matching what the tailer thread uses in production.
const POLL: Duration = Duration::from_millis(250);

/// How often the roster is rebuilt, matching `RESCAN_EVERY * POLL_INTERVAL`.
const RESCAN: Duration = Duration::from_secs(2);

#[test]
#[ignore = "needs a real Claude Code session; see this file's module docs"]
fn a_real_agent_started_in_another_terminal_is_discovered_with_zero_setup() {
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

    // The watch starts FIRST, with nothing running in the repository. This is
    // the ordering an operator actually uses and the one the old design could
    // not serve: open the map, then start an agent.
    let mapper = PathMapper::new(&repo).expect("the repository path must be mappable");
    let mut tailer = LiveTailer::open(&projects, Scope::Repo(mapper));
    let before = tailer.roster();
    eprintln!(
        "watching {} — {} at the start",
        repo.display(),
        before.headline()
    );
    assert!(
        before.here().filter(|s| s.activity.is_live()).count() == 0,
        "start this against a repository with no agent already in it: {}",
        before.headline()
    );

    let mut agent = spawn_agent_if_asked(&repo);

    let started = Instant::now();
    let mut events: Vec<Event> = Vec::new();
    let mut last_rescan = Instant::now();
    let mut found: Option<String> = None;
    let mut last_fit = Fit::Unknown;
    while started.elapsed() < DEADLINE {
        if last_rescan.elapsed() >= RESCAN {
            tailer.rescan();
            last_rescan = Instant::now();
            let roster = tailer.roster();
            // Every session born during this watch, whether or not its `cwd` has
            // landed yet: a brand-new transcript is empty for a moment, and the
            // roster deliberately refuses to claim it for this repository until
            // a record says so. Waiting for `here()` is waiting for that.
            let fresh = roster
                .resolving()
                .chain(roster.here())
                .find(|s| s.started_while_watching && s.activity.is_live())
                .map(|s| (s.session.as_str().to_owned(), s.line(), s.fit));
            if let Some((id, line, fit)) = fresh {
                if found.as_deref() != Some(id.as_str()) || fit != last_fit {
                    eprintln!("  {line}");
                    found = Some(id);
                    last_fit = fit;
                }
            }
        }
        let before = events.len();
        tailer.poll(&mut events);
        if events.len() > before {
            eprintln!(
                "  +{} events ({} total)",
                events.len() - before,
                events.len()
            );
        }
        // Done when the session has resolved to *this* repository and has
        // produced work — presence alone is not the claim being made.
        if last_fit == Fit::Inside && events.len() >= 8 {
            break;
        }
        std::thread::sleep(POLL);
    }
    if let Some(child) = agent.as_mut() {
        let _ = child.wait();
    }

    // --- what the operator asked for --------------------------------------
    let session = found.expect(
        "no session started in this repository was discovered. Start `claude` in it while \
         this test runs, or set POLIS_ACCEPTANCE_SPAWN=1",
    );
    check(&tailer, &session, &repo, &events, started.elapsed());
}

/// Everything the acceptance test asserts, once the agent has been seen.
fn check(tailer: &LiveTailer, session: &str, repo: &Path, events: &[Event], elapsed: Duration) {
    let roster = tailer.roster();
    let discovered = roster
        .here()
        .find(|s| s.session.as_str() == session)
        .expect(
            "the session was seen, but never resolved to the repository being watched — the \
         roster refuses to claim a session for a repository until one of its records names \
         that directory",
        );
    assert!(
        discovered.started_while_watching,
        "a session that began after the watch must be marked as such"
    );
    assert!(
        discovered.tailing,
        "a live in-scope session must be followed, not merely listed"
    );
    let reported = discovered
        .cwd
        .as_deref()
        .map(|cwd| cwd.replace('\\', "/").to_lowercase());
    let wanted = repo.display().to_string().replace('\\', "/").to_lowercase();
    assert_eq!(
        reported.as_deref(),
        Some(wanted.as_str()),
        "the session must resolve to the repository being watched"
    );

    // Presence is half the answer; the events are the other half.
    assert!(
        !events.is_empty(),
        "the session was discovered but produced no events"
    );
    let transcript = events
        .iter()
        .filter(|e| matches!(e.payload, Payload::Transcript(_)))
        .count();
    eprintln!(
        "discovered {session} and read {transcript} transcript events in {elapsed:?} — no \
         hooks, no telemetry environment, no wrapper"
    );
    assert!(transcript > 0, "no transcript records reached the bus");
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

/// Starts `claude -p` in the repository, when asked to.
///
/// One tiny prompt, and only on request: this costs an operator's API quota, and
/// the default is that they start the agent themselves in another terminal —
/// which is a stricter test anyway, because nothing about that process knows
/// Polis exists.
fn spawn_agent_if_asked(repo: &Path) -> Option<std::process::Child> {
    if std::env::var_os("POLIS_ACCEPTANCE_SPAWN").is_none() {
        eprintln!("  now start `claude` in {} …", repo.display());
        return None;
    }
    let child = std::process::Command::new(if cfg!(windows) {
        "claude.cmd"
    } else {
        "claude"
    })
    .current_dir(repo)
    .args(["-p", "Reply with exactly: ok"])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::inherit())
    .stderr(std::process::Stdio::inherit())
    .spawn();
    match child {
        Ok(child) => {
            eprintln!("  spawned claude in {}", repo.display());
            Some(child)
        }
        Err(error) => {
            eprintln!("  could not spawn claude ({error}); start one by hand");
            None
        }
    }
}
