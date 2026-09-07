//! PRD §10.4's cloud cap, measured against a real fleet.
//!
//! > **Cap the number of visible clouds.** Forty threads means forty systems and
//! > the map vanishes under haze. Render clouds only for threads with an active
//! > attention state or in the top N by recent activity; let dormant territories
//! > dissipate entirely.
//!
//! PRD §17 lists this as a named risk — *"the cap policy will need tuning against
//! real fleets"* — and its first open question asks for the number. This is the
//! tuning. It builds a fleet by replaying the operator's own sessions **into one
//! world at once**, which is what the operator actually asked for (*"I want to
//! see all agents active in a repository on the machine"*), and measures two
//! things on the rendered field:
//!
//! * **coverage** — the fraction of the map inside a cloud band. This is fog.
//! * **contested fraction** — of the covered cells, how many are claimed by two
//!   or more territories at once. This is the legibility number, and it is the
//!   one that decides the cap: a cell claimed by four threads answers *"work is
//!   happening"* and refuses to answer *"whose"*, which is the aggregate
//!   illusion PRD §17 names as this genre's default failure.
//!
//! ```text
//! cargo test -p polis-render --release --test cloud_cap_policy -- --ignored --nocapture
//! ```
//!
//! # It tunes the cap **at the fleet's busiest moment**, deliberately
//!
//! [`build_fleet`] aligns every session on its own `pacing::plan` peak and
//! measures there, and its own doc says why: a fleet sampled anywhere else is
//! mostly dead air and the cap has nothing to decide. That is right for tuning a
//! *cap* — a cap only ever binds when many territories are live at once — and it
//! is worth saying out loud that it is therefore a measurement of the best case
//! and not of a typical minute. On a fixed session-time grid the same replays put
//! a cloud on the map 62 % of the time against 94 % under event-weighted
//! sampling, and nothing in this file would notice the difference.
//!
//! So a green run here says *the cap is set at the right crossover when the
//! crossover happens*. It does not say the map has five clouds on it most of the
//! time. See `cloud_measure.rs`'s header for the same caveat on the other
//! harness.

// A measurement harness: counts, ratios, and one linear script per measurement.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use polis_events::PathMapper;
use polis_layout::city::City;
use polis_render::live::{self, CloudKernel, LiveFrame};
use polis_render::plan::{self, MapFrame};
use polis_render::raster::Canvas;
use polis_world::replay::ReplayDriver;
use polis_world::sessions::{IndexOptions, SessionIndex};
use polis_world::territory::{self, CloudPolicy, Territory};
use polis_world::{DenylistUbiquity, Thread, World};

/// Caps to compare.
const CAPS: [usize; 12] = [1, 2, 3, 4, 5, 6, 8, 10, 12, 16, 24, 40];

/// Fleet sizes to compare, from PRD §10.4's own example.
const FLEETS: [usize; 4] = [1, 3, 10, 40];

/// Where the evidence images go.
fn out_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("docs")
        .join("m4-cloud-cap");
    std::fs::create_dir_all(&dir).expect("out dir");
    dir
}

/// A fleet of `n` real sessions, replayed concurrently into one world, each
/// aligned so its busiest stretch overlaps everyone else's.
///
/// Alignment is the whole trick. Real sessions are mostly dead air (PRD §15 M2's
/// reason for compressing idle gaps), so replaying ten of them from their own
/// starts gives ten mostly-idle threads and measures nothing. `pacing::plan`
/// already knows where each session's work is; offsetting each driver's origin
/// by that puts every session's busiest window on the same clock.
struct Fleet {
    world: World,
    /// Session-time seconds elapsed in the shared window.
    elapsed: f64,
}

fn build_fleet(
    city: &polis_layout::CityLayout,
    sessions: &[(PathBuf, PathBuf)],
    run_for: Duration,
) -> Option<Fleet> {
    let mut world = World::for_replay(city.clone());
    world.set_ubiquity(Box::new(DenylistUbiquity::default()));

    // `peak` is each session's busiest moment, and the fleet is measured **at**
    // it, not after it. Running past the peak and sampling there measures a
    // fleet that has gone home: PRD §6.3's 90 s half-life had ten minutes to
    // work and every field had decayed to nothing. The run-up is what fills the
    // territories; the measurement instant is the peak itself.
    let mut loaded: Vec<(polis_world::replay::ReplaySchedule, u64)> = Vec::new();
    for (repo, dir) in sessions {
        let Ok(mapper) = PathMapper::new(repo) else {
            continue;
        };
        let Ok(mut schedule) = polis_world::replay::ReplaySchedule::from_session_dir(dir, &mapper)
        else {
            continue;
        };
        schedule.compress_idle_gaps(None);
        if schedule.is_empty() {
            continue;
        }
        let peak = polis_render::pacing::plan(&schedule, 2)
            .start_ms
            .min(schedule.duration_ms());
        loaded.push((schedule, peak));
    }
    if loaded.is_empty() {
        return None;
    }
    // Every driver's origin is rebased so that its own peak sits on one shared
    // instant; without that, ten sessions replayed from their own starts are ten
    // mostly-idle threads and the fleet measures nothing. The shared instant is
    // pushed into the future by the largest offset so no origin lands before the
    // process started.
    let furthest = loaded.iter().map(|(_, p)| *p).max().unwrap_or(0);
    let shared = Instant::now() + Duration::from_millis(furthest);
    let mut rebased: Vec<(ReplayDriver, u64)> = loaded
        .into_iter()
        .map(|(schedule, peak)| {
            // `shared` was pushed forward by the largest peak, so this cannot
            // land before the process started; `checked_sub` says so anyway.
            let origin = shared
                .checked_sub(Duration::from_millis(peak))
                .unwrap_or(shared);
            (ReplayDriver::with_origin(schedule, origin), peak)
        })
        .collect();

    let step = Duration::from_secs(5);
    let mut behind = run_for;
    loop {
        for (driver, peak) in &mut rebased {
            let target = peak.saturating_sub(behind.as_millis() as u64);
            driver.seek(target, &mut world);
        }
        if behind.is_zero() {
            break;
        }
        behind = behind.saturating_sub(step);
    }
    Some(Fleet {
        world,
        elapsed: run_for.as_secs_f64(),
    })
}

/// What one cap did to one fleet.
#[derive(Debug, Clone, Copy)]
struct CapStats {
    shown: usize,
    unplaced: usize,
    dormant: usize,
    capped: usize,
    kernels: usize,
    /// Map cells inside any band, over map cells.
    coverage: f64,
    /// Covered cells claimed by two or more territories, over covered cells.
    contested: f64,
    /// Mean number of territories claiming a covered cell.
    mean_crowd: f64,
}

fn project(visible: &[&Territory], view: &plan::View) -> Vec<CloudKernel> {
    let mut out = Vec::new();
    for (rank, territory) in visible.iter().enumerate() {
        for k in &territory.kernels {
            if k.weight <= 0.0 {
                continue;
            }
            out.push(CloudKernel {
                at: view.at(k.centre),
                radius: f64::from(k.radius) * view.scale(),
                weight: f64::from(k.weight),
                thread: u16::try_from(rank).unwrap_or(u16::MAX),
            });
        }
    }
    out
}

fn measure(
    world: &World,
    view: &plan::View,
    pixels: usize,
    cap: usize,
) -> (CapStats, Vec<CloudKernel>) {
    let pairs: Vec<(&Thread, &Territory)> =
        world.threads.values().map(|t| (t, &t.territory)).collect();
    let selection = territory::select_clouds(
        &pairs,
        &world.attention,
        CloudPolicy::default().with_cap(cap),
    );
    let kernels = project(&selection.visible, view);
    let bands = live::band_map(&kernels, pixels, pixels);
    let total = (pixels * pixels) as f64;
    let (mut covered, mut contested, mut crowd_sum) = (0usize, 0usize, 0u64);
    if let Some(bands) = &bands {
        for y in 0..bands.height {
            for x in 0..bands.width {
                if bands.at(bands.x0 + x, bands.y0 + y) == live::NO_BAND {
                    continue;
                }
                covered += 1;
                let c = bands.crowd_at(bands.x0 + x, bands.y0 + y);
                crowd_sum += u64::from(c);
                if c >= 2 {
                    contested += 1;
                }
            }
        }
    }
    (
        CapStats {
            shown: selection.visible.len(),
            unplaced: selection.unplaced,
            dormant: selection.dormant,
            capped: selection.capped,
            kernels: kernels.len(),
            coverage: covered as f64 / total,
            contested: contested as f64 / covered.max(1) as f64,
            mean_crowd: crowd_sum as f64 / covered.max(1) as f64,
        },
        kernels,
    )
}

/// Base map, then clouds, then labels — PRD §10.3's layer order.
fn compose(base: &MapFrame, kernels: &[CloudKernel], unit: f64, pixels: usize) -> Canvas {
    let mut canvas = base.map.clone();
    let frame = LiveFrame {
        unit,
        map_height: pixels as f64,
        clouds: kernels.to_vec(),
        ..LiveFrame::default()
    };
    live::draw_clouds(&mut canvas, &frame);
    for (i, colour) in &base.labels {
        let i = *i as usize * 3;
        if i + 3 <= canvas.pixels.len() {
            canvas.pixels[i..i + 3].copy_from_slice(colour);
        }
    }
    canvas
}

#[test]
#[ignore = "reads the operator's real sessions"]
fn the_cloud_cap_against_a_real_fleet() {
    let Some(projects) = polis_world::sessions::default_projects_dir().filter(|p| p.is_dir())
    else {
        eprintln!("skipped: no ~/.claude/projects on this machine");
        return;
    };
    let index = SessionIndex::scan_with(&projects, &IndexOptions::quick()).expect("scan");

    // One repository, so one city and one set of coordinates. The busiest
    // project on this machine is the fleet.
    let mut by_repo: BTreeMap<PathBuf, Vec<(u64, PathBuf, PathBuf)>> = BTreeMap::new();
    for s in &index.sessions {
        if !s.repo_exists || !s.is_replayable() {
            continue;
        }
        let Some(repo) = s.repo.clone() else { continue };
        by_repo
            .entry(repo.clone())
            .or_default()
            .push((s.bytes, repo, s.session_dir()));
    }
    let Some((repo_path, all)) = by_repo.iter_mut().max_by_key(|(_, v)| v.len()) else {
        eprintln!("skipped: no replayable sessions");
        return;
    };
    // Biggest first: a fleet of forty two-record sessions is forty rail entries,
    // not forty clouds, and would measure nothing about fog.
    all.sort_by_key(|(bytes, _, _)| std::cmp::Reverse(*bytes));
    let sessions: Vec<(PathBuf, PathBuf)> = all
        .iter()
        .map(|(_, repo, dir)| (repo.clone(), dir.clone()))
        .collect();
    eprintln!(
        "fleet repository {} with {} replayable sessions",
        repo_path.display(),
        sessions.len()
    );

    let repo = polis_repo::tree::RepoIndex::open(repo_path).expect("index");
    let built = Instant::now();
    let city: City = polis_layout::city::generate_city(repo.tree());
    let layout = city.layout.clone();
    eprintln!(
        "city: {} buildings in {:?}",
        layout.buildings.len(),
        built.elapsed()
    );

    let pixels = 720usize;
    let city_frame = plan::render_map_frame(&city, pixels, 2, false, None);
    let out = out_dir();

    for fleet_size in FLEETS {
        let take: Vec<(PathBuf, PathBuf)> = sessions.iter().take(fleet_size).cloned().collect();
        let fleet_size = take.len();
        let started = Instant::now();
        let Some(fleet) = build_fleet(&layout, &take, Duration::from_secs(600)) else {
            eprintln!("fleet of {fleet_size}: nothing replayed");
            continue;
        };
        let live_threads = fleet.world.threads.len();
        let converged = fleet
            .world
            .threads
            .values()
            .filter(|t| t.territory.claim.is_some() && !t.territory.kernels.is_empty())
            .count();
        eprintln!(
            "\nfleet of {fleet_size} sessions -> {live_threads} threads ({converged} with a field), \
             {:.0}s of run-up to the shared peak, built in {:?}",
            fleet.elapsed,
            started.elapsed()
        );
        for thread in fleet.world.threads.values() {
            let c = thread.territory.convergence();
            eprintln!(
                "    {:<10} {:<8} ops {:>4} evidence {:>3} kernels {:>3} depth {} mass {:.2} \
                 quiet {:>5.0}s claim {}",
                thread
                    .session_id
                    .as_str()
                    .chars()
                    .take(8)
                    .collect::<String>(),
                thread.status.to_string(),
                thread.ops.len(),
                c.observations,
                thread.territory.kernels.len(),
                c.depth,
                c.mass_ratio,
                thread
                    .territory
                    .quiet_for()
                    .map_or(f64::NAN, |q| q.as_secs_f64()),
                thread
                    .territory
                    .claim
                    .as_ref()
                    .map_or("-", polis_events::LogicalPath::as_str),
            );
        }
        let pairs: Vec<(&Thread, &Territory)> = fleet
            .world
            .threads
            .values()
            .map(|t| (t, &t.territory))
            .collect();

        // PRD §12's district tier, framed on the work. The city tier flatters
        // the cap: on a repository with `node_modules` in it the source is a
        // corner of the map and every cloud is a smudge. The tier the operator
        // works at is the one the notation has to survive.
        let district = work_focus(&pairs);
        let district_frame =
            district.map(|f| plan::render_map_frame(&city, pixels, 2, false, Some(f)));

        for (tier, frame) in [
            ("city", Some(&city_frame)),
            ("work", district_frame.as_ref()),
        ] {
            let Some(frame) = frame else {
                eprintln!("  {tier}: no territory to frame on");
                continue;
            };
            eprintln!(
                "  {tier} tier\n  {:>4} {:>6} {:>8} {:>8} {:>7} {:>9} {:>10} {:>10}",
                "cap", "shown", "unplaced", "dormant", "capped", "kernels", "coverage", "contested"
            );
            for cap in CAPS {
                let (stats, kernels) = measure(&fleet.world, &frame.view, pixels, cap);
                eprintln!(
                    "  {:>4} {:>6} {:>8} {:>8} {:>7} {:>9} {:>9.2}% {:>9.1}%  crowd {:.2}",
                    cap,
                    stats.shown,
                    stats.unplaced,
                    stats.dormant,
                    stats.capped,
                    stats.kernels,
                    stats.coverage * 100.0,
                    stats.contested * 100.0,
                    stats.mean_crowd,
                );
                if tier == "work" && (cap == 1 || cap == territory::CLOUD_CAP || cap == 40) {
                    let canvas = compose(frame, &kernels, frame.unit, pixels);
                    let path = out.join(format!("fleet-{fleet_size:02}-cap-{cap:02}.png"));
                    canvas.write_png(&path).expect("write png");
                    eprintln!("       -> {}", path.display());
                }
            }

            // What dormancy alone buys, with the cap lifted entirely.
            for window in [30u64, 90, 180, 600, 86_400] {
                let policy = CloudPolicy {
                    cap: usize::MAX,
                    dormant_after: Duration::from_secs(window),
                };
                let sel = territory::select_clouds(&pairs, &fleet.world.attention, policy);
                let kernels = project(&sel.visible, &frame.view);
                let bands = live::band_map(&kernels, pixels, pixels);
                let covered = bands.as_ref().map_or(0, |b| {
                    (0..b.height)
                        .flat_map(|y| (0..b.width).map(move |x| (x, y)))
                        .filter(|(x, y)| b.at(b.x0 + x, b.y0 + y) != live::NO_BAND)
                        .count()
                });
                eprintln!(
                    "  dormant_after {window:>6}s (no cap): {:>3} shown, {:>3} dissipated, coverage {:>5.2}%",
                    sel.visible.len(),
                    sel.dormant,
                    covered as f64 / (pixels * pixels) as f64 * 100.0,
                );
            }
        }
    }
}

/// The frame the cap is judged in: every live territory, and nothing else.
///
/// One frame for every cap, so the images differ **only** in how many clouds
/// were admitted. Framed on the work rather than on the whole city because the
/// city tier flatters the cap — on a repository with `node_modules` in it the
/// source is a corner of the map and every cloud is a smudge — and PRD §12's
/// district tier is where the operator actually works.
fn work_focus(pairs: &[(&Thread, &Territory)]) -> Option<plan::Focus> {
    let live: Vec<&Territory> = pairs
        .iter()
        .map(|(_, t)| *t)
        .filter(|t| t.claim.is_some() && !t.kernels.is_empty())
        .collect();
    if live.is_empty() {
        return None;
    }
    let (mut lo, mut hi) = ((f32::MAX, f32::MAX), (f32::MIN, f32::MIN));
    let mut bandwidth = 0.0f32;
    for t in &live {
        bandwidth = bandwidth.max(t.bandwidth());
        for k in &t.kernels {
            lo.0 = lo.0.min(k.centre.x - k.radius);
            lo.1 = lo.1.min(k.centre.y - k.radius);
            hi.0 = hi.0.max(k.centre.x + k.radius);
            hi.1 = hi.1.max(k.centre.y + k.radius);
        }
    }
    let centre = polis_layout::Point::new(f32::midpoint(lo.0, hi.0), f32::midpoint(lo.1, hi.1));
    let extent = ((hi.0 - lo.0).max(hi.1 - lo.1) / 2.0).max(bandwidth * 2.0);
    Some(plan::Focus { centre, extent })
}
