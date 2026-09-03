//! PRD §6.4 on a real orchestrator.
//!
//! The operator watched a live map of this repository with one agent working on
//! it — this session, 101 workers, 3 386 path-bearing calls across 18 top-level
//! directories — and said "i dont see any clouds". They were right, and it was
//! §6.2 behaving exactly as written: the weight-trimmed ancestor of evidence
//! spread that wide is the repository root, depth 0, so nothing converged and
//! nothing drew.
//!
//! §6.4 describes the intended picture for precisely this thread: *"An agent
//! working in `auth` with one worker in `tests` gets two lobes and a thin
//! connecting band."* The two sections disagree, and this test pins the
//! resolution: the depth-and-mass gate is applied per cluster, so an
//! orchestrator gets lobes where a single ancestor gets nothing.

use std::time::{Duration, Instant};

use polis_events::{LogicalPath, SessionId, ThreadId, ToolKind};
use polis_world::territory::{Territory, MIN_CLAIM_DEPTH, MIN_LOBE_MASS};
use polis_world::{Observation, PathScope};

fn lp(s: &str) -> LogicalPath {
    LogicalPath::new(s).expect("logical path")
}

/// One `Read` of a file, which PRD §6.2 credits to its parent directory.
fn read(path: &str, at: std::time::Instant) -> Observation {
    Observation {
        thread: ThreadId::of_session(SessionId::new("t")),
        worker: None,
        path: lp(path),
        tool: ToolKind::Read,
        scope: PathScope::File,
        at,
        weight: None,
    }
}

/// The shape of this session's real evidence: heavy work in several crates at
/// once, plus the scattering of top-level files that drags the ancestor to root.
fn orchestrator() -> Territory {
    let mut t = Territory::default();
    let now = Instant::now();
    let spread = [
        ("polis-layout/src/roads.rs", 30),
        ("polis-app/src/mapview.rs", 20),
        ("polis-world/src/territory.rs", 16),
        ("polis-render/src/live.rs", 14),
        ("polis-repo/src/llm.rs", 12),
        ("docs/PRD.md", 8),
    ];
    for (path, n) in spread {
        for i in 0..n {
            t.observe(&read(path, now + Duration::from_millis(i)), 1.0, None);
        }
    }
    // The strays that collapse a single ancestor to the root.
    t.observe(&read("README.md", now), 1.0, None);
    t.observe(&read("Cargo.toml", now), 1.0, None);
    t
}

#[test]
fn a_single_ancestor_cannot_describe_an_orchestrator() {
    let t = orchestrator();
    let c = t.convergence();
    assert!(
        c.claim.is_none(),
        "expected §6.2's single ancestor to fail on spread evidence, got {:?}",
        c.claim
    );
    assert_eq!(c.depth, 0, "the trimmed ancestor is the repository root");
}

#[test]
fn but_it_has_lobes_and_they_are_where_the_work_is() {
    let t = orchestrator();
    let c = t.convergence();
    assert!(
        c.lobes.len() >= 4,
        "expected several lobes, got {:?}",
        c.lobes.iter().map(|l| l.path.as_str()).collect::<Vec<_>>()
    );
    for lobe in &c.lobes {
        assert_eq!(
            lobe.path.depth(),
            MIN_CLAIM_DEPTH,
            "a lobe is exactly the gate's depth, not deeper or shallower"
        );
        assert!(lobe.mass >= MIN_LOBE_MASS, "a lobe carries real weight");
    }
    // The lobes are the crates the thread actually worked in. Which one comes
    // out heaviest depends on how §6.2's trim happens to fall across equal
    // weights, and that is not the contract — being *present* is.
    let paths: Vec<&str> = c.lobes.iter().map(|l| l.path.as_str()).collect();
    for expected in ["polis-layout/src", "polis-app/src", "polis-world/src"] {
        assert!(
            paths.contains(&expected),
            "expected {expected} among {paths:?}"
        );
    }
    // Masses are descending, so a renderer drawing them in order draws the
    // biggest first.
    for w in c.lobes.windows(2) {
        assert!(
            w[0].mass >= w[1].mass,
            "lobes are heaviest-first: {paths:?}"
        );
    }
    // The top-level strays raise nothing: they cannot reach depth 2.
    assert!(
        !c.lobes.iter().any(|l| l.path.as_str().contains("README")),
        "a top-level file must not raise a lobe"
    );
}

#[test]
fn a_focused_thread_still_gets_exactly_one_lobe_and_a_claim() {
    let mut t = Territory::default();
    let now = Instant::now();
    for i in 0..12 {
        t.observe(
            &read("src/auth/login.rs", now + Duration::from_millis(i)),
            1.0,
            None,
        );
    }
    let c = t.convergence();
    assert!(
        c.claim.is_some(),
        "a focused thread still converges normally"
    );
    assert_eq!(c.lobes.len(), 1, "and reads as one place, not many");
}
