//! Measurement harness for the live layer, run against the operator's own
//! sessions (PRD §17: *"does it change a decision?"* needs a number).
//!
//! Everything here is `#[ignore]`d: it reads `~/.claude/projects` and a real
//! checkout, neither of which exists on a build machine. It writes no images —
//! `polis_render::frame`'s `record_a_real_session` does that — it writes
//! **counts**, because the three things M2 got wrong (an empty attention band,
//! an invisible backtrack, a static replay) are all things a screenshot cannot
//! disprove and a histogram can.
//!
//! ```text
//! POLIS_SESSION=6f51089f POLIS_PACE=derived \
//!   cargo test -p polis-render --release --test replay_measure -- --ignored --nocapture
//! ```
//!
//! | env | meaning |
//! |---|---|
//! | `POLIS_SESSION` | substring of the session id; default `6f51089f` |
//! | `POLIS_PACE` | `derived` (default) or `legacy` — the fixed-window sampling M2 shipped |
//! | `POLIS_FRAMES` / `POLIS_PIXELS` | frame count and map size |

// A measurement harness: every number here is a count or a millisecond count
// divided by another, and the one function is one long linear script on
// purpose — splitting it would hide the order the measurements are taken in.
#![allow(clippy::cast_precision_loss, clippy::too_many_lines)]

use std::time::{Duration, Instant};

use polis_events::PathMapper;
use polis_render::frame::{self, FrameOptions, FrameRenderer};
use polis_render::live::{self, TrailStyle};
use polis_render::pacing;
use polis_render::plan::{AGENT_BAND, ATTENTION_BAND};
use polis_render::raster::Canvas;
use polis_world::replay::ReplayDriver;
use polis_world::sessions::{IndexOptions, SessionIndex};
use polis_world::{snapshot, DenylistUbiquity, World};

struct BandStats {
    attention: usize,
    agent: usize,
    changed: usize,
}

fn stats(now: &Canvas, prev: Option<&Canvas>, map_rows: usize) -> BandStats {
    let row = now.width * 3;
    let end = (map_rows * row).min(now.pixels.len());
    let mut attention = 0;
    let mut agent = 0;
    let mut changed = 0;
    for i in (0..end).step_by(3) {
        let v = now.pixels[i..i + 3].iter().copied().max().unwrap_or(0);
        if v >= ATTENTION_BAND.0 {
            attention += 1;
        } else if v >= AGENT_BAND.0 {
            agent += 1;
        }
        if let Some(p) = prev {
            if p.pixels[i..i + 3] != now.pixels[i..i + 3] {
                changed += 1;
            }
        }
    }
    BandStats {
        attention,
        agent,
        changed,
    }
}

#[test]
#[ignore = "reads the operator's real sessions"]
fn measure_a_real_replay() {
    let Some(projects) = polis_world::sessions::default_projects_dir().filter(|p| p.is_dir())
    else {
        eprintln!("skipped: no ~/.claude/projects on this machine");
        return;
    };
    let index = SessionIndex::scan_with(&projects, &IndexOptions::quick()).expect("scan");
    let want = std::env::var("POLIS_SESSION").unwrap_or_else(|_| "6f51089f".to_owned());
    let mut candidates: Vec<_> = index
        .sessions
        .iter()
        .filter(|s| s.repo_exists && s.repo.is_some())
        .filter(|s| s.session.as_str().contains(&want))
        .collect();
    candidates.sort_by_key(|s| std::cmp::Reverse(s.bytes));
    let Some(sample) = candidates.first() else {
        eprintln!("skipped: no session matching {want}");
        return;
    };
    let repo_path = sample.repo.clone().expect("a repo");
    let repo = polis_repo::tree::RepoIndex::open(&repo_path).expect("index");
    let city = polis_layout::city::generate_city(repo.tree());
    let mapper = PathMapper::new(&repo_path).expect("mapper");
    let mut schedule = sample.schedule(&mapper).expect("read");
    schedule.compress_idle_gaps(Some(polis_world::replay::DEFAULT_IDLE_GAP_CAP));

    let frames: usize = std::env::var("POLIS_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(96);
    let pixels: usize = std::env::var("POLIS_PIXELS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600);
    let legacy = std::env::var("POLIS_PACE").as_deref() == Ok("legacy");

    let plan = pacing::plan(&schedule, frames);
    // What M2 shipped: a fixed 45 s window of the compressed timeline, cut into
    // `frames` equal slices, with nothing checking the result.
    let legacy_ms: Vec<u64> = {
        let width = 45_000u64.min(schedule.duration_ms());
        let start = plan
            .start_ms
            .min(schedule.duration_ms().saturating_sub(width));
        (0..frames)
            .map(|i| start + width * i as u64 / (frames as u64 - 1))
            .collect()
    };
    let placements: &[u64] = if legacy { &legacy_ms } else { &plan.frames_ms };

    eprintln!(
        "session {} — {} events ({} live), {:.1} min compressed",
        sample.session,
        schedule.len(),
        plan.total_live_events,
        schedule.duration_ms() as f64 / 60_000.0,
    );
    eprintln!(
        "pacing[{}]: {} frames, mean {:.3} world-s/frame, window {:.2}-{:.2} min, \
         {} live events + {} decisions inside, {} at the floor, {} at the ceiling — {}",
        if legacy { "legacy" } else { "derived" },
        placements.len(),
        (placements[placements.len() - 1].saturating_sub(placements[0])) as f64
            / (placements.len() - 1) as f64
            / 1000.0,
        placements[0] as f64 / 60_000.0,
        placements[placements.len() - 1] as f64 / 60_000.0,
        plan.live_events,
        plan.decisions,
        plan.floored,
        plan.ceilinged,
        plan.verdict(),
    );

    let mut world = World::new(repo.tree().clone(), city.layout.clone());
    world.set_ubiquity(Box::new(DenylistUbiquity::default()));
    let mut driver = ReplayDriver::with_origin(schedule.clone(), Instant::now());
    let mut renderer = FrameRenderer::new(
        &city,
        FrameOptions {
            pixels,
            supersample: 1,
            trail: TrailStyle::Timed,
            caption: false,
            ..FrameOptions::default()
        },
    );
    let (publisher, reader) = snapshot::from_world(&world);
    let mut prev: Option<Canvas> = None;
    let mut attn_frames = 0usize;
    let mut attn_px_max = 0usize;
    let mut attn_px_total = 0usize;
    let mut static_frames = 0usize;
    let mut dead_frames = 0usize;
    let mut marks_seen = 0usize;
    let mut trail_max = 0usize;
    let mut returns_max = 0usize;
    let mut returns_total = 0usize;
    let mut return_frames = 0usize;
    let mut budget: Vec<Duration> = Vec::new();
    for (i, target) in placements.iter().copied().enumerate() {
        driver.seek(target, &mut world);
        publisher.force(&world);
        let snap = reader.load();
        marks_seen = marks_seen.max(snap.attention.len());
        trail_max = trail_max.max(
            snap.threads
                .iter()
                .map(|t| t.trail.len())
                .max()
                .unwrap_or(0),
        );
        // PRD §12's backtrack, counted the way the renderer sees it: a trail
        // leg whose road has been walked before is a **return**, and before the
        // bow it was drawn on top of the outbound leg and therefore invisible.
        let view = *renderer.view();
        for t in &snap.threads {
            let steps: Vec<live::TrailStep> = t
                .trail
                .iter()
                .filter_map(|(p, at)| {
                    frame::place(snap.layout.as_ref(), p, &view).map(|at_px| live::TrailStep {
                        at: at_px,
                        age: snap.at.saturating_duration_since(*at).as_secs_f64(),
                        visits: t.revisits(p),
                    })
                })
                .collect();
            let returns = live::leg_repeats(&steps).iter().filter(|n| **n > 0).count();
            returns_max = returns_max.max(returns);
            returns_total += returns;
            if returns > 0 {
                return_frames += 1;
            }
        }
        let canvas = renderer.render_owned(&snap, Duration::from_millis(1000 / 25));
        budget.push(renderer.timings().budgeted());
        let s = stats(&canvas, prev.as_ref(), pixels);
        if s.attention > 0 {
            attn_frames += 1;
        }
        attn_px_max = attn_px_max.max(s.attention);
        attn_px_total += s.attention;
        // The M2 post-mortem's threshold, scaled: 200 changed pixels of a
        // 1.32M-pixel frame, so the number is comparable at any map size.
        if prev.is_some() && s.changed * 1_320_000 < 200 * pixels * pixels {
            static_frames += 1;
        }
        if s.agent + s.attention == 0 {
            dead_frames += 1;
        }
        if i % 16 == 0 {
            let t = &snap.threads[0];
            eprintln!(
                "  frame {i:>3} @{:>7} ms: {:>5} attention px, {:>6} agent px, {:>7} changed, \
                 {} marks | ops {} trail {} visits {} workers {} files {}",
                target,
                s.attention,
                s.agent,
                s.changed,
                snap.attention.len(),
                t.ops.len(),
                t.trail.len(),
                t.visits.len(),
                t.workers.len(),
                snap.files.len(),
            );
        }
        prev = Some(canvas);
    }
    budget.sort_unstable();
    let area = pixels * pixels;
    eprintln!(
        "RESULT frames={} attention_frames={} ({:.1}%) attention_px_max={} ({:.4}% of map) \
         attention_px_mean={:.1} static_frames={} ({:.0}%) dead_frames={} ({:.0}%) \
         max_marks={} max_trail={} return_frames={} ({:.0}%) returns_max={} returns_mean={:.1}          draw_p50={:?} draw_p95={:?}",
        placements.len(),
        attn_frames,
        100.0 * attn_frames as f64 / placements.len() as f64,
        attn_px_max,
        100.0 * attn_px_max as f64 / area as f64,
        attn_px_total as f64 / placements.len() as f64,
        static_frames,
        100.0 * static_frames as f64 / placements.len() as f64,
        dead_frames,
        100.0 * dead_frames as f64 / placements.len() as f64,
        marks_seen,
        trail_max,
        return_frames,
        100.0 * return_frames as f64 / placements.len() as f64,
        returns_max,
        returns_total as f64 / placements.len() as f64,
        budget[budget.len() / 2],
        budget[budget.len() * 95 / 100],
    );
}
