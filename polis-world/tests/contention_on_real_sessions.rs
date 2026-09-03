//! Contention, measured against the operator's **real** sessions (PRD §11.3).
//!
//! > TEST with real data, not mocks. (PRD §16)
//!
//! The defect this file exists to hold shut had two halves, and both of them
//! made PRD §11.1's top-ranked state — *the only one where work is actively
//! being destroyed* — unable to fire in the arrangement that produces it most
//! often, which is **two workers of one session**:
//!
//! 1. Claims were keyed by [`polis_events::ThreadId`], and a thread is a session
//!    plus its whole worker subtree, so a second claim from a sibling worker
//!    refreshed the first instead of colliding with it.
//! 2. A claim was **deleted** when its write landed, and in a transcript a tool
//!    result follows its tool call by well under a second. Two workers 4.7 s
//!    apart therefore never held a claim at the same instant, so even with the
//!    key fixed there was nothing left to collide.
//!
//! Both are measured here rather than asserted. The scan behind the numbers is
//! `docs/verified/jsonl-schema.md`'s corpus; the two sessions named below are
//! the operator's own, and every test skips with a printed reason when they are
//! not on the machine so a bare CI runner stays green.
//!
//! Run with `-- --nocapture` to see the counts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use polis_events::{LogicalPath, PathMapper, SessionId};
use polis_world::attention::AttentionKind;
use polis_world::contention::{Actor, ContentionPrecision, Severity};
use polis_world::replay::{ReplayDriver, ReplaySchedule};
use polis_world::sessions::{IndexOptions, SessionIndex, SessionSummary};
use polis_world::World;

/// The session in which two subagents wrote `settings.css` 4.7 s apart.
///
/// Three distinct workers wrote that one file; the closest pair is 4.698 s, well
/// inside [`polis_world::contention::CLAIM_TTL`], on one branch and in one
/// checkout. That is `High` at least, and it is the collision that thread-keying
/// rendered as nothing at all.
const SETTINGS_CSS_SESSION: &str = "9cab97d7-6166-4ae7-8b3f-a076567a2412";

/// The session with the large multi-worker fan-out.
///
/// Scanned 2026-09-02: 95 logical paths written by more than one worker, 82
/// distinct writers. (The 77 in the original measurement is the same session,
/// counted earlier — it is still running, so the number only grows.)
const FANOUT_SESSION: &str = "6f51089f-1ec0-4e78-9bc9-8ff6864e0100";

/// How often the world is ticked, in session time.
///
/// Fifteen times under [`polis_world::contention::CLAIM_TTL`], so nothing this
/// file is looking for can pass between two samples. Coarser than a frame on
/// purpose: this replays days of session time, and a 60 Hz cadence over that
/// spends minutes re-deciding a question whose answer changes on the order of
/// tens of seconds. Measured — 200 ms, 1 s and 5 s all find the same
/// collisions.
const TICK: Duration = Duration::from_secs(2);

/// The other tick gate: at most this many events between two ticks.
///
/// A transcript's timestamps are data, not a clock. One record with a far-future
/// timestamp pins [`World::now`] there — `World::apply` only ever moves it
/// forward — and a purely time-based cadence then stops firing for the rest of
/// the read. That is not hypothetical: it is what made the first run of this
/// file report zero contentions while the claim table held four live claims on
/// `settings.css`.
const TICK_EVENTS: u32 = 64;

/// Above this many contended paths in one session, the number is the finding.
///
/// PRD §17: *"does it change a decision? If not, cut it."* A handful of red
/// links in a session says *go look at these files*; sixty says the detector is
/// broken, and an operator who learns that is an operator who stops looking.
/// The bound is deliberately loose — the sessions here contain one and three —
/// because what it is guarding against is an order of magnitude, not a count.
const MAX_PLAUSIBLE_COLLISIONS: usize = 16;

fn projects_dir() -> Option<PathBuf> {
    let dir = polis_world::sessions::default_projects_dir()?;
    dir.is_dir().then_some(dir)
}

/// Scanned once per process: the corpus is over a thousand files.
fn index() -> Option<Arc<SessionIndex>> {
    static CACHE: OnceLock<Option<Arc<SessionIndex>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let projects = projects_dir()?;
            SessionIndex::scan_with(&projects, &IndexOptions::quick())
                .ok()
                .map(Arc::new)
        })
        .clone()
}

fn session(index: &SessionIndex, id: &str) -> Option<SessionSummary> {
    index.get(&SessionId::new(id)).cloned()
}

/// A schedule whose clock is the session's own, subagents included.
///
/// # Why this is not `SessionSummary::schedule`
///
/// **Measured, not assumed.** `polis_ingest`'s offline reader concatenates the
/// main transcript and every subagent file in `(file, byte_offset)` order and
/// then runs *one* monotonising running maximum over the whole concatenation
/// (ADR-0014, ADR-0049). The main transcript comes first, so by the time the
/// first subagent file is reached the running maximum is already the session's
/// **last** timestamp — and every subagent record after it is clamped to that
/// one instant.
///
/// The consequence for anything with a clock in it is severe, and contention is
/// the sharpest case: with all 82 workers' writes landing at the same
/// millisecond, a 30 s TTL holds every one of them live at once. Replaying
/// `6f51089f` that way reported **126 contentions on 58 paths**, where the same
/// data under PRD §11.3's rule contains **3 on 3 paths**. Forty times too much
/// red is not a contention layer; it is the aggregate illusion PRD §17 names.
///
/// So each file is read on its own — where the running maximum is correct, being
/// confined to one append-ordered file — and the results are merged on their
/// absolute wall clock. The fix belongs in `polis-ingest`; this reads around it
/// so the contention numbers below are about contention.
fn session_schedule(sample: &SessionSummary, mapper: &PathMapper) -> Option<ReplaySchedule> {
    let listing = sample.schedule(mapper).ok()?;
    let mut events: Vec<polis_events::RecordedEvent> = Vec::new();
    for file in &listing.files {
        let Ok(one) = ReplaySchedule::from_transcript(&file.path, mapper) else {
            continue;
        };
        events.extend(one.entries.into_iter().map(|e| e.event));
    }
    events.sort_by_key(|e| e.wall.unix_millis());
    let origin = events.first().map_or(0, |e| e.wall.unix_millis());
    for e in &mut events {
        e.monotonic_offset_ms = u64::try_from(e.wall.unix_millis().saturating_sub(origin)).ok()?;
    }
    Some(ReplaySchedule::from_events(
        listing.header.clone(),
        events,
        listing.stats.clone(),
        listing.files.clone(),
    ))
}

/// What one replay of one real session found.
///
/// Everything here is `Send + Sync`, so a whole replay can be run once and
/// shared: reading `9cab97d7` and `6f51089f` costs about a minute apiece, and
/// three tests that each replayed both would spend five minutes proving the same
/// two things.
#[derive(Debug)]
struct Replayed {
    /// Every distinct contention seen at any point during the replay, keyed by
    /// the pair of actors and the path they were fighting over.
    seen: BTreeMap<(Actor, Actor, LogicalPath), (Severity, ContentionPrecision)>,
    /// Of those, the ones whose two ends are workers of one session.
    within_thread: usize,
    /// Contended paths where the link joined two *different* map positions.
    ///
    /// PRD §11.2c wants a link, and a link needs two places. Counted rather than
    /// assumed, because when both agents are working on the one file they are
    /// fighting over there is only one place, and the model must say so.
    links_with_two_ends: usize,
    events: usize,
    ticks: u64,
    /// Claim-table and health counters from the end of the replay.
    claim_paths: usize,
    table_hits: u64,
    paths_evicted: u64,
    over_cap: u64,
    degraded: u64,
    /// Edits whose diff had to be approximated. Not a contention number.
    edits_degraded: u64,
}

/// One replay per session per process.
fn replayed(sample: &SessionSummary) -> Option<Arc<Replayed>> {
    static CACHE: OnceLock<Mutex<BTreeMap<String, Option<Arc<Replayed>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    let key = sample.session.as_str().to_owned();
    let mut guard = cache.lock().expect("replay cache");
    if let Some(hit) = guard.get(&key) {
        return hit.clone();
    }
    let value = replay(sample).map(Arc::new);
    guard.insert(key, value.clone());
    value
}

/// Replays a whole real session — main transcript plus every subagent file —
/// and records every contention the attention layer raised along the way.
///
/// The world is ticked on a cadence rather than once at the end, because
/// contention is a **relation between two live claims** and a relation that has
/// expired is not supposed to still be there: sampling only the final state
/// would see an empty table and conclude, wrongly, that nothing ever collided.
fn replay(sample: &SessionSummary) -> Option<Replayed> {
    let repo_path = sample.repo.clone()?;
    let repo = polis_repo::tree::RepoIndex::open(&repo_path).ok()?;
    let layout = polis_layout::city::generate(repo.tree());
    let mapper = PathMapper::new(&repo_path).ok()?;
    let schedule = session_schedule(sample, &mapper)?;
    let mut world = World::new(repo.tree().clone(), layout);
    let origin = Instant::now();
    let mut driver = ReplayDriver::with_origin(schedule, origin);

    let mut seen: BTreeMap<(Actor, Actor, LogicalPath), (Severity, ContentionPrecision)> =
        BTreeMap::new();
    let mut two_ended: BTreeSet<LogicalPath> = BTreeSet::new();
    let mut within_thread = 0;
    let mut events = 0;
    // Stepped one event at a time so `world.now` advances by real session time
    // and the 30 s TTL means what it says — but **ticked on a cadence**, which
    // is PRD §5's *"never render on event arrival"* and also what a window does.
    // A relation that lives 30 s cannot hide between two samples a quarter of a
    // second apart, and sampling only the final state would see an empty table
    // and conclude, wrongly, that nothing ever collided.
    let mut next_tick = world.now();
    let mut ticks = 0_u64;
    let mut since_tick = 0_u32;
    while driver.step(&mut world) {
        events += 1;
        let now = world.now();
        since_tick += 1;
        // Two gates, and the second is not redundant. A transcript's timestamps
        // are not a clock Polis controls: a single record with a far-future one
        // pins `World::now` there for the rest of the read, and a purely
        // time-based cadence then never fires again. The event gate keeps the
        // sampling honest whatever the file says.
        if now < next_tick && since_tick < TICK_EVENTS {
            continue;
        }
        next_tick = now + TICK;
        since_tick = 0;
        ticks += 1;
        world.tick(now);
        for mark in &world.attention {
            let AttentionKind::Contention(hit) = &mark.kind else {
                continue;
            };
            let (a, b) = hit.actors();
            let key = (a, b, hit.path().clone());
            let entry = seen.entry(key).or_insert((hit.severity, hit.precision));
            entry.0 = entry.0.max(hit.severity);
            if hit.is_within_thread() {
                within_thread += 1;
            }
            // PRD §11.2c, exercised rather than asserted: resolve the relation
            // to two ends the way a renderer has to.
            let link = hit.link(&world.layout, |id| world.thread(id));
            assert_eq!(link.within_thread, hit.is_within_thread());
            if !link.is_degenerate() {
                two_ended.insert(hit.path().clone());
            }
        }
    }
    Some(Replayed {
        seen,
        within_thread,
        links_with_two_ends: two_ended.len(),
        events,
        ticks,
        claim_paths: world.claims.len(),
        table_hits: world.claims.total_hits(),
        paths_evicted: world.claims.paths_evicted(),
        over_cap: world.health.contention_over_cap,
        degraded: world.health.contention_without_line_ranges,
        edits_degraded: world.health.edits_without_line_ranges,
    })
}
fn report(name: &str, r: &Replayed) {
    eprintln!(
        "  {name}: {} events, {} ticks, {} distinct contentions, \
         {} within-thread sightings, {} links with two ends",
        r.events,
        r.ticks,
        r.seen.len(),
        r.within_thread,
        r.links_with_two_ends
    );
    let mut by_severity: BTreeMap<Severity, usize> = BTreeMap::new();
    for (severity, _) in r.seen.values() {
        *by_severity.entry(*severity).or_default() += 1;
    }
    for (severity, n) in &by_severity {
        eprintln!(
            "    {:>8} ({}): {n}",
            format!("{severity:?}"),
            severity.label()
        );
    }
    for ((a, b, path), (severity, precision)) in &r.seen {
        eprintln!(
            "    {path}\n      {} vs {} -> {severity:?} / {precision:?}",
            name_of(a),
            name_of(b)
        );
    }
    eprintln!(
        "    claims: {} paths live at the end, {} hits registered, {} paths evicted",
        r.claim_paths, r.table_hits, r.paths_evicted
    );
    eprintln!(
        "    health: contention_over_cap={} hits_without_line_ranges={}          edits_without_line_ranges={}",
        r.over_cap, r.degraded, r.edits_degraded
    );
}

fn name_of(actor: &Actor) -> String {
    match &actor.worker {
        Some(w) => format!("worker {}", w.as_str()),
        None => "main agent".to_owned(),
    }
}

/// Loads and replays one named session, or prints why it could not.
fn named(id: &str) -> Option<Arc<Replayed>> {
    let index = index().or_else(|| {
        eprintln!("skipped: no ~/.claude/projects on this machine");
        None
    })?;
    let sample = session(&index, id).or_else(|| {
        eprintln!("skipped: session {id} is not on this machine");
        None
    })?;
    if !sample.is_replayable() {
        eprintln!("skipped: {id}'s repository is gone");
        return None;
    }
    replayed(&sample).or_else(|| {
        eprintln!("skipped: could not open the repository for {id}");
        None
    })
}

#[test]
fn the_settings_css_collision_fires() {
    let Some(r) = named(SETTINGS_CSS_SESSION) else {
        return;
    };
    report("settings.css session", &r);

    let hits: Vec<_> = r
        .seen
        .iter()
        .filter(|((_, _, path), _)| path.as_str().ends_with("settings.css"))
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "the data contains exactly one pair of workers writing settings.css \
         inside the 30 s TTL, and thread-keying saw none of them"
    );
    let ((a, b, _), (severity, precision)) = hits[0];
    assert_eq!(
        a.thread, b.thread,
        "both writers are workers of one session — the case a ThreadId-keyed \
         table cannot represent at all"
    );
    assert!(
        a.worker.is_some() && b.worker.is_some() && a.worker != b.worker,
        "two distinct subagents: {a:?} vs {b:?}"
    );
    assert_eq!(
        *severity,
        Severity::High,
        "one session is one checkout on one branch, so it is `same file`"
    );
    assert_eq!(
        *precision,
        ContentionPrecision::FileLevel,
        "a subagent result carries no structuredPatch on 65% of calls, so the \
         line-range tier is unavailable and the mark must say so (ADR-0004)"
    );
    assert_eq!(
        r.seen.len(),
        1,
        "and nothing else in this session collided: over-firing contention is \
         the aggregate illusion PRD §17 names, in the one colour the map \
         reserves for work being destroyed"
    );
}

#[test]
fn the_multi_worker_fan_out_produces_the_contention_the_data_contains() {
    let Some(r) = named(FANOUT_SESSION) else {
        return;
    };
    report("fan-out session", &r);

    let paths: BTreeSet<&LogicalPath> = r.seen.keys().map(|(_, _, p)| p).collect();
    let within: BTreeSet<&LogicalPath> = r
        .seen
        .keys()
        .filter(|(a, b, _)| a.thread == b.thread)
        .map(|(_, _, p)| p)
        .collect();
    assert!(
        !within.is_empty(),
        "an 82-worker session wrote 95 logical paths with more than one worker; \
         the pairs inside the TTL must render"
    );
    assert_eq!(
        paths, within,
        "every collision in a single-session replay is worker-versus-worker, \
         which is exactly the arrangement the old key could not see"
    );
    // Not pinned to an exact count: this is the operator's **live** session, so
    // it grows between runs. What is pinned is the shape, and the shape is what
    // was wrong. Cross-checked on 2026-09-03 against an independent model of
    // PRD §11.3 written straight off the JSONL, sharing no code with
    // `polis-world`: both said three hits on the same three paths
    // (`polis-render/src/live.rs`, `polis-world/src/apply.rs`,
    // `polis-app/src/cli.rs`) out of the 95 paths two workers wrote at *some*
    // point in this session's life.
    assert!(
        paths.len() <= MAX_PLAUSIBLE_COLLISIONS,
        "{} contended paths in one session is not a contention layer, it is the          aggregate illusion PRD §17 names — in the one colour the map reserves          for work being destroyed. An earlier build reported 58 here, because          the offline reader had collapsed every subagent record onto a single          instant: {paths:?}",
        paths.len()
    );
    assert_eq!(r.paths_evicted, 0, "no collision went unobservable");
    assert_eq!(r.over_cap, 0, "and none was pushed off the attention layer");
}

/// The counterfactual, stated as the number it was.
#[test]
fn every_real_collision_is_one_a_thread_keyed_table_could_not_represent() {
    let mut total = 0;
    let mut within = 0;
    let mut replayed_any = false;
    for id in [SETTINGS_CSS_SESSION, FANOUT_SESSION] {
        let Some(r) = named(id) else { continue };
        replayed_any = true;
        for (a, b, _) in r.seen.keys() {
            total += 1;
            if a.thread == b.thread {
                within += 1;
            }
        }
    }
    if !replayed_any {
        return;
    }
    eprintln!("  {within} of {total} real contentions are worker-versus-worker inside one session");
    assert!(total > 0, "the operator's corpus does contain collisions");
    assert_eq!(
        within, total,
        "worker-versus-worker is not an edge case; on this corpus it is the \
         only case, and a table keyed by ThreadId rendered exactly none of it"
    );
}
