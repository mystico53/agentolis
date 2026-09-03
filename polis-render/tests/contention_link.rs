//! PRD §11.2c on **pixels**: contention is a link between two places, and the
//! two-workers-of-one-session case is the one that must not collapse to a dot.
//!
//! > **(c) Contention** — red. This is a **relation between two threads, not a
//! > property of one**, so it is drawn as a link joining them across the map,
//! > not a badge on a dot. It is also the only state that can pull the eye to
//! > two places at once. (PRD §11.2)
//!
//! `polis_world::contention` already carries the machinery for this:
//! [`polis_world::contention::Contention::link`] places each end with
//! `place::agent_position`, which puts a *worker* at its own focus, and
//! [`polis_world::contention::ContentionLink::is_degenerate`] says when the two
//! nevertheless coincide. Its own doc comment names the defect it was built for:
//!
//! > It exists because the renderer previously had only `Contention::threads()`
//! > to work with, and two workers of one session give that the **same id
//! > twice** — which draws a link of zero length, which is to say a badge on a
//! > dot, which is the one thing PRD §11.2 says contention must never be.
//!
//! This file is the assertion that the renderer actually uses it. It matters
//! more than the cross-session case, because **every** contention in the
//! operator's real corpus is worker-versus-worker inside one session
//! (`polis-world/tests/contention_on_real_sessions.rs`: 4 of 4).

use std::time::{Duration, Instant};

use polis_events::{LogicalPath, SessionId, ThreadId, WorkerId, WorktreeId};
use polis_layout::city::{self, City};
use polis_render::frame::{FrameOptions, FrameRenderer};
use polis_render::live::MarkKind;
use polis_repo::{synthetic, RepoTree};
use polis_world::attention::{Attention, AttentionKind};
use polis_world::contention::Claim;
use polis_world::{snapshot, Thread, Worker, World};

fn city() -> City {
    city::generate_city(&synthetic::repository(240, 0x51))
}

/// Two logical paths far enough apart in the city to be two places, plus the
/// file they collide over.
fn far_apart(c: &City) -> (LogicalPath, LogicalPath, LogicalPath) {
    let mut files: Vec<LogicalPath> = c.layout.buildings.keys().cloned().collect();
    files.sort();
    assert!(files.len() > 8, "the synthetic city must have buildings");
    let a = files[0].clone();
    let b = files[files.len() - 1].clone();
    let target = files[files.len() / 2].clone();
    (a, b, target)
}

/// A session whose two workers are focused on two different parts of the city
/// and are both writing one file.
fn world_with_within_thread_contention(c: &City) -> (World, ThreadId) {
    let mut world = World::new(RepoTree::default(), c.layout.clone());
    let (focus_a, focus_b, target) = far_apart(c);
    let t0 = Instant::now();
    let session = SessionId::new("6f51089f-1ec0-4e78-9bc9-8ff6864e0100");
    let id = ThreadId::of_session(session.clone());

    let mut thread = Thread::new(id.clone(), session, t0);
    thread.trail.push_back((target.clone(), t0));
    for (wid, focus) in [("w-alpha", &focus_a), ("w-beta", &focus_b)] {
        let mut w = Worker::new(WorkerId::new(wid), t0);
        w.focus = Some(focus.clone());
        thread.workers.push(w);
    }
    world.threads.insert(id.clone(), thread);

    let first = Claim::write(id.clone(), target.clone(), t0)
        .in_checkout(WorktreeId::PRIMARY, Some("main"))
        .by_worker(Some(WorkerId::new("w-alpha")));
    assert!(
        world.claims.claim(first).is_none(),
        "the first is not a hit"
    );
    let second = Claim::write(id.clone(), target, t0)
        .in_checkout(WorktreeId::PRIMARY, Some("main"))
        .by_worker(Some(WorkerId::new("w-beta")));
    let hit = world
        .claims
        .claim(second)
        .expect("two workers of one session on one file is a hit");
    assert!(
        hit.is_within_thread(),
        "the case under test is two workers of ONE session"
    );
    world
        .attention
        .push(Attention::new(AttentionKind::Contention(Box::new(hit)), t0));
    (world, id)
}

/// **The defect.** The contention mark the renderer emits must name two
/// *different* places when the world knows two, or the link is a dot.
#[test]
fn two_workers_of_one_session_are_two_places_and_not_one_dot() {
    let c = city();
    let (world, _) = world_with_within_thread_contention(&c);

    let mut r = FrameRenderer::new(
        &c,
        FrameOptions {
            pixels: 600,
            supersample: 1,
            caption: false,
            rail: false,
            ..FrameOptions::default()
        },
    );
    let (_, reader) = snapshot::from_world(&world);
    let snap = reader.load();
    let frame = r.build_frame(&snap, Duration::from_millis(40));

    let mark = frame
        .attention
        .iter()
        .find(|m| m.kind == MarkKind::Contention)
        .expect("the contention must reach the frame at all");
    let other = mark.other.expect("a contention mark must have two ends");
    let d = (mark.at[0] - other[0]).hypot(mark.at[1] - other[1]);
    eprintln!(
        "  within-thread link: {:?} -> {:?}  ({d:.1} px apart)",
        mark.at, other
    );
    assert!(
        d > 2.0,
        "PRD §11.2c: two workers of one session, focused on two different \
         districts, drew a link {d:.1} px long — a badge on a dot. \
         `Contention::link` places each end by worker focus; the renderer is \
         not using it."
    );
}

/// The other half, and the one that keeps the fix honest: when both claimants
/// really are in one place, the link stays a dot.
///
/// > True when Polis knows only one position — both agents working on the file
/// > they are fighting over […]. Not a failure: it is the difference between
/// > *"these two are in different parts of the city"* and *"these two are on
/// > top of each other"*, and the second is the more alarming of the two.
/// > (`ContentionLink::is_degenerate`)
///
/// This is the shape of the **live** two-agent run in
/// `polis-world/tests/live_contention.rs`, where both agents were pointed at one
/// file and nothing else: it reported `degenerate: true`, and that was correct.
/// Separating the ends here would be inventing a position, which ADR-0004
/// forbids.
#[test]
fn two_agents_on_one_file_and_nothing_else_stay_one_place() {
    let c = city();
    let mut world = World::new(RepoTree::default(), c.layout.clone());
    let (_, _, target) = far_apart(&c);
    let t0 = Instant::now();

    for name in ["session-a", "session-b"] {
        let session = SessionId::new(name);
        let id = ThreadId::of_session(session.clone());
        let mut thread = Thread::new(id.clone(), session, t0);
        thread.trail.push_back((target.clone(), t0));
        world.threads.insert(id, thread);
    }
    let mut hit = None;
    for name in ["session-a", "session-b"] {
        let id = ThreadId::of_session(SessionId::new(name));
        let claim =
            Claim::write(id, target.clone(), t0).in_checkout(WorktreeId::PRIMARY, Some("main"));
        hit = world.claims.claim(claim).or(hit);
    }
    let hit = hit.expect("two sessions on one file is a hit");
    assert!(!hit.is_within_thread(), "two sessions, two threads");
    world
        .attention
        .push(Attention::new(AttentionKind::Contention(Box::new(hit)), t0));

    let mut r = FrameRenderer::new(
        &c,
        FrameOptions {
            pixels: 600,
            supersample: 1,
            caption: false,
            rail: false,
            ..FrameOptions::default()
        },
    );
    let (_, reader) = snapshot::from_world(&world);
    let snap = reader.load();
    let frame = r.build_frame(&snap, Duration::from_millis(40));
    let mark = frame
        .attention
        .iter()
        .find(|m| m.kind == MarkKind::Contention)
        .expect("the contention must reach the frame");
    let other = mark.other.expect("two ends");
    let d = (mark.at[0] - other[0]).hypot(mark.at[1] - other[1]);
    eprintln!("  one-place link: {d:.1} px apart");
    assert!(
        d < 2.0,
        "both threads know only the contended file, so there is one place; \
         drawing them {d:.1} px apart would be a position Polis invented"
    );
}
