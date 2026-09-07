//! Where every operation ends up on the map — the PRD §10.1/§10.2 census.
//!
//! An operation the map cannot place is an operation the operator cannot see,
//! and failure happens overwhelmingly on the tools that carry no file path:
//! measured over one project's whole corpus, 374 of 544 tool failures are on
//! `Bash` or `PowerShell`. So "how many ops did we place, on which rung, by
//! tool, and with which outcome" is a number this repository has to be able to
//! print on demand.
//!
//! Ignored by default: it reads the operator's real `~/.claude/projects`.
//!
//! ```sh
//! POLIS_SESSION=29c2fc6f cargo test --release -p polis-world \
//!     --test placement_census -- --ignored --nocapture
//! ```

// A census prints percentages, so `u64 as f64` is on every line of it and the
// precision the lint is protecting is irrelevant at these magnitudes.
#![allow(clippy::cast_precision_loss)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use polis_events::{Glyph, LogicalPath, Outcome, PathMapper, ToolUseId};
use polis_world::place::site_of;
use polis_world::replay::ReplayDriver;
use polis_world::sessions::{IndexOptions, SessionIndex, SessionSummary};
use polis_world::{DenylistUbiquity, World};

/// One operation, as the world last held it.
#[derive(Clone)]
struct Seen {
    tool: String,
    glyph: Glyph,
    outcome: Outcome,
    /// Rung 1–4 of `place`'s chain.
    rung: u8,
    /// Whether the **old** rule would have drawn it: `op.path` alone, resolved
    /// straight to geometry, with no fallback under it.
    legacy: bool,
}

fn projects_dir() -> Option<PathBuf> {
    polis_world::sessions::default_projects_dir().filter(|p| p.is_dir())
}

fn pick(index: &SessionIndex, want: Option<&str>) -> Option<SessionSummary> {
    let mut c: Vec<&SessionSummary> = index
        .sessions
        .iter()
        .filter(|s| s.repo_exists && s.repo.is_some())
        .filter(|s| want.is_none_or(|w| s.session.as_str().contains(w)))
        .collect();
    c.sort_by_key(|s| std::cmp::Reverse(s.bytes));
    c.first().map(|s| (*s).clone())
}

/// Replays a real session and prints the placement census.
#[test]
#[ignore = "reads the operator's real sessions"]
fn a_real_session_places_every_operation() {
    let Some(projects) = projects_dir() else {
        eprintln!("skipped: no ~/.claude/projects on this machine");
        return;
    };
    let index = SessionIndex::scan_with(&projects, &IndexOptions::quick()).expect("scan");
    let want = std::env::var("POLIS_SESSION").ok();
    let Some(sample) = pick(&index, want.as_deref()) else {
        eprintln!("skipped: no replayable session");
        return;
    };
    let repo_path = sample.repo.clone().expect("a repo");
    eprintln!(
        "session {} ({} KiB) over {}",
        sample.session,
        sample.bytes / 1024,
        repo_path.display()
    );

    let repo = polis_repo::tree::RepoIndex::open(&repo_path).expect("index the repository");
    let layout = polis_layout::city::generate(repo.tree());
    eprintln!(
        "  city: {} buildings, {} districts, root district present: {}",
        layout.buildings.len(),
        layout.districts.len(),
        layout.districts.contains_key(&LogicalPath::root())
    );

    let mapper = PathMapper::new(&repo_path).expect("mapper");
    let mut schedule = sample.schedule(&mapper).expect("offline full read");
    schedule.compress_idle_gaps(Some(polis_world::replay::DEFAULT_IDLE_GAP_CAP));
    eprintln!("  {} events", schedule.len());

    let mut world = World::new(repo.tree().clone(), layout);
    world.set_ubiquity(Box::new(DenylistUbiquity::default()));
    let mut driver = ReplayDriver::with_origin(schedule, Instant::now());

    // Ops are capped and faded, so they have to be harvested as they go. Keyed
    // on `tool_use_id`, which every transcript `tool_use` block carries, and
    // overwritten each pass so the last state seen is the one recorded — which
    // is exactly what the renderer would have had.
    let mut seen: BTreeMap<ToolUseId, Seen> = BTreeMap::new();
    let mut untracked = 0u64;
    let started = Instant::now();
    loop {
        let more = driver.step(&mut world);
        harvest(&world, &mut seen, &mut untracked);
        if !more {
            break;
        }
    }
    eprintln!("  replayed in {:?}", started.elapsed());

    report(&seen, untracked, &world);
}

fn harvest(world: &World, seen: &mut BTreeMap<ToolUseId, Seen>, untracked: &mut u64) {
    for thread in world.threads.values() {
        for op in &thread.ops {
            let Some(id) = op.tool_use.clone() else {
                *untracked += 1;
                continue;
            };
            seen.insert(
                id,
                Seen {
                    tool: op.tool.to_string(),
                    glyph: op.glyph,
                    outcome: op.outcome,
                    rung: site_of(op, thread, &world.layout).rung(),
                    legacy: op
                        .path
                        .as_ref()
                        .is_some_and(|p| world.position_of(p).is_some()),
                },
            );
        }
    }
}

#[allow(clippy::too_many_lines)] // a census is a table; splitting it hides the totals
fn report(seen: &BTreeMap<ToolUseId, Seen>, untracked: u64, world: &World) {
    // [total, rung1, rung2, rung3, rung4, pending, done, failed, legacy-drawn]
    let mut by_tool: BTreeMap<&str, [u64; 9]> = BTreeMap::new();
    let mut totals = [0u64; 9];
    let mut failed_by_rung = [0u64; 4];
    let mut legacy_failed = 0u64;
    let mut glyphs: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut legacy_glyphs: BTreeMap<&'static str, u64> = BTreeMap::new();

    for s in seen.values() {
        let name = match s.glyph {
            Glyph::HollowCircle => "read",
            Glyph::BarredCircle => "edit",
            Glyph::FilledSquare => "write",
            Glyph::FilledTriangle => "run",
            Glyph::ConcentricCircles => "verify",
            Glyph::Delegate => "delegate",
        };
        *glyphs.entry(name).or_default() += 1;
        if s.legacy {
            *legacy_glyphs.entry(name).or_default() += 1;
        }
        if s.outcome == Outcome::Failed {
            failed_by_rung[usize::from(s.rung) - 1] += 1;
            if s.legacy {
                legacy_failed += 1;
            }
        }
        let row = by_tool.entry(s.tool.as_str()).or_insert([0; 9]);
        for r in [row, &mut totals] {
            r[0] += 1;
            r[usize::from(s.rung)] += 1;
            r[match s.outcome {
                Outcome::Pending => 5,
                Outcome::Done => 6,
                Outcome::Failed => 7,
            }] += 1;
            if s.legacy {
                r[8] += 1;
            }
        }
    }

    let mut rows: Vec<(&str, [u64; 9])> = by_tool.into_iter().collect();
    rows.sort_by_key(|(_, r)| std::cmp::Reverse(r[0]));

    eprintln!();
    let head = format!(
        "{:<38} {:>6} {:>7} {:>6} {:>6} {:>8} | {:>7} {:>6} {:>6} | {:>6}",
        "tool", "ops", "r1 path", "r2 cwd", "r3 agt", "r4 rail", "pending", "done", "FAIL", "was"
    );
    eprintln!("{head}");
    for (tool, r) in &rows {
        eprintln!(
            "{:<38} {:>6} {:>7} {:>6} {:>6} {:>8} | {:>7} {:>6} {:>6} | {:>6}",
            tool, r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7], r[8]
        );
    }
    eprintln!(
        "{:<38} {:>6} {:>7} {:>6} {:>6} {:>8} | {:>7} {:>6} {:>6} | {:>6}",
        "TOTAL",
        totals[0],
        totals[1],
        totals[2],
        totals[3],
        totals[4],
        totals[5],
        totals[6],
        totals[7],
        totals[8]
    );
    let placed = totals[1] + totals[2] + totals[3];
    let pct = |n: u64| 100.0 * n as f64 / totals[0].max(1) as f64;
    eprintln!();
    eprintln!(
        "  placed now {placed}/{} ({:.1}%); placed before {} ({:.1}%); rail {} ({:.1}%); ops with no tool_use id {untracked}",
        totals[0],
        pct(placed),
        totals[8],
        pct(totals[8]),
        totals[4],
        pct(totals[4]),
    );
    eprintln!(
        "  failures placed now {} of {} (rung 1 {}, 2 {}, 3 {}, RAIL {}); placed before {}",
        failed_by_rung[0] + failed_by_rung[1] + failed_by_rung[2],
        totals[7],
        failed_by_rung[0],
        failed_by_rung[1],
        failed_by_rung[2],
        failed_by_rung[3],
        legacy_failed,
    );
    eprintln!("  glyphs now {glyphs:?}");
    eprintln!("  glyphs before {legacy_glyphs:?}");
    eprintln!("  health: ops {:?}", world.health.ops);
    eprintln!("  health: failed {:?}", world.health.ops_failed);
    eprintln!(
        "  health: results that arrived after their op was evicted: {}",
        world.health.ops_settled_after_eviction
    );
    assert!(totals[0] > 0, "a real session performs operations");
    // Rung 4 is not zero and should not be: the first calls of a session happen
    // before the thread has touched anything the city has geometry for, so they
    // genuinely have nowhere to go. What must hold is that they are *counted*
    // and that the map is no longer losing most of the session.
    assert_eq!(
        placed + totals[4],
        totals[0],
        "an operation went missing between the chain's rungs"
    );
    assert!(
        placed * 20 >= totals[0] * 19,
        "only {placed} of {} operations reached the map",
        totals[0]
    );
    assert!(
        placed >= totals[8],
        "the chain placed fewer operations than the rule it replaced"
    );
}
