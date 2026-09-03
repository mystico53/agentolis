//! The measurement the notation is judged by: what colour is the live layer?
//!
//! PRD §10.2 gives the colour channel three states — pending, done, failed —
//! and a map on which every mark is teal has thrown two of them away. This
//! replays a real session, renders it headlessly, and classifies every
//! agent-band pixel (`plan::AGENT_BAND`, channels 97–168) against the eight
//! inks `live` actually uses. `live::fade` scales all three channels by one
//! factor, so a mark's **hue ratio** survives ageing exactly and nearest-hue is
//! a sound classifier.
//!
//! Ignored by default: it reads the operator's real `~/.claude/projects`.
//!
//! ```sh
//! POLIS_SESSION=29c2fc6f cargo test --release -p polis-render \
//!     --test colour_census -- --ignored --nocapture
//! ```

// A census prints percentages, so `u64 as f64` is on every line of it and the
// precision the lint is protecting is irrelevant at these magnitudes.
#![allow(clippy::cast_precision_loss)]
// Pixel indices and frame counts are the same story.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names
)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use polis_events::{Glyph, Outcome, PathMapper};
use polis_layout::Point;
use polis_render::frame::{position_of, FrameOptions, FrameRenderer, MARK_TTL};
use polis_render::live::{self, LiveFrame, Mark, PULSE_SECS};
use polis_render::plan::Focus;
use polis_render::raster::Canvas;
use polis_world::replay::ReplayDriver;
use polis_world::sessions::{IndexOptions, SessionIndex};
use polis_world::{snapshot, DenylistUbiquity, World};

/// The inks `polis_render::live` draws in the agent band, as `(name, rgb)`.
const INKS: [(&str, [u8; 3]); 8] = [
    ("pending", [132, 140, 156]),
    ("done", [96, 168, 150]),
    ("failed", [168, 98, 92]),
    ("tether", [100, 110, 130]),
    ("trail", [110, 122, 144]),
    ("body", [150, 158, 168]),
    ("anchor", [138, 146, 162]),
    ("scaffold", [140, 148, 112]),
];

/// Chroma above which a pixel counts as "saturated" — carrying hue rather than
/// being one of the neutrals.
const SATURATED: f64 = 0.18;

fn norm(p: [f64; 3]) -> [f64; 3] {
    let m = p[0].max(p[1]).max(p[2]).max(1.0);
    [p[0] / m, p[1] / m, p[2] / m]
}

fn classify(p: [u8; 3]) -> &'static str {
    let v = norm([f64::from(p[0]), f64::from(p[1]), f64::from(p[2])]);
    let mut best = ("", f64::INFINITY);
    for (name, ink) in INKS {
        let q = norm([f64::from(ink[0]), f64::from(ink[1]), f64::from(ink[2])]);
        let d = (v[0] - q[0]).powi(2) + (v[1] - q[1]).powi(2) + (v[2] - q[2]).powi(2);
        if d < best.1 {
            best = (name, d);
        }
    }
    best.0
}

/// The marks the **old** rule produced, reproduced exactly: an operation was
/// drawn only if `op.path` resolved to geometry, the newest 48 per thread, and
/// nothing under that.
///
/// Reproduced in the test rather than measured from an older build, so both
/// numbers come out of one binary and neither the trail notation nor
/// `MARK_TTL` can move between them.
fn legacy_marks(
    snap: &polis_world::snapshot::WorldSnapshot,
    view: &polis_render::plan::View,
) -> Vec<Mark> {
    const MARKS_PER_THREAD: usize = 48;
    let layout = snap.layout.as_ref();
    let mut out = Vec::new();
    for thread in &snap.threads {
        for op in thread.ops.iter().rev().take(MARKS_PER_THREAD) {
            let Some(path) = op.path.as_ref() else {
                continue;
            };
            let Some(p) = position_of(layout, path) else {
                continue;
            };
            let age = snap.at.saturating_duration_since(op.at).as_secs_f64();
            if age > MARK_TTL {
                continue;
            }
            let pulse = if age >= PULSE_SECS {
                0.0
            } else {
                1.0 - age / PULSE_SECS
            };
            out.push(Mark::single(
                view.at(p),
                op.glyph,
                op.outcome,
                age / MARK_TTL,
                pulse,
            ));
        }
    }
    out
}

/// Ink census of a marks-only layer drawn on a blank canvas.
///
/// Marks alone, because that is the channel under test: trail and tether ink
/// outweigh it by area and are identical under both rules, so including them
/// would only dilute the comparison.
fn marks_only(marks: &[Mark], unit: f64, pixels: usize, into: &mut BTreeMap<&'static str, u64>) {
    let frame = LiveFrame {
        unit,
        map_height: pixels as f64,
        marks: marks.to_vec(),
        ..LiveFrame::default()
    };
    let mut canvas = Canvas::new(pixels, pixels, [0, 0, 0]);
    live::draw_agents(&mut canvas, &frame, live::TrailStyle::Timed);
    for p in canvas.pixels.as_chunks::<3>().0 {
        let hi = p.iter().copied().max().unwrap_or(0);
        if !(97..=168).contains(&hi) {
            continue;
        }
        *into.entry(classify(*p)).or_default() += 1;
    }
}

#[test]
#[ignore = "reads the operator's real sessions"]
#[allow(clippy::too_many_lines)] // a census is a table; splitting it hides the totals
fn the_agent_band_is_not_all_teal() {
    let Some(projects) = polis_world::sessions::default_projects_dir().filter(|p| p.is_dir())
    else {
        eprintln!("skipped: no ~/.claude/projects on this machine");
        return;
    };
    let index = SessionIndex::scan_with(&projects, &IndexOptions::quick()).expect("scan");
    let want = std::env::var("POLIS_SESSION").unwrap_or_default();
    let mut c: Vec<_> = index
        .sessions
        .iter()
        .filter(|s| s.repo_exists && s.repo.is_some())
        .filter(|s| s.session.as_str().contains(&want))
        .collect();
    c.sort_by_key(|s| std::cmp::Reverse(s.bytes));
    let Some(sample) = c.first().map(|s| (*s).clone()) else {
        eprintln!("skipped: no replayable session");
        return;
    };
    let repo_path = sample.repo.clone().expect("a repo");
    eprintln!("session {} over {}", sample.session, repo_path.display());

    let repo = polis_repo::tree::RepoIndex::open(&repo_path).expect("index the repository");
    let city = polis_layout::city::generate_city(repo.tree());
    let mapper = PathMapper::new(&repo_path).expect("mapper");
    let mut schedule = sample.schedule(&mapper).expect("offline full read");
    schedule.compress_idle_gaps(Some(polis_world::replay::DEFAULT_IDLE_GAP_CAP));
    eprintln!(
        "  {} buildings, {} districts, {} events",
        city.layout.buildings.len(),
        city.layout.districts.len(),
        schedule.len()
    );

    let env_num = |k: &str, d: f64| -> f64 {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let frames = env_num("POLIS_FRAMES", 120.0) as usize;
    let pixels = env_num("POLIS_PIXELS", 900.0) as usize;

    // The recorder's own window rule: the span of compressed timeline over which
    // the session reaches the most distinct paths. Duplicated here rather than
    // shared because the recorder is a `#[test]` in the crate's own source.
    let window = (env_num("POLIS_WINDOW_S", 45.0) * 1000.0) as u64;
    let mut probe = World::for_replay(city.layout.clone());
    let mut scout = ReplayDriver::with_origin(schedule.clone(), Instant::now());
    let mut reached: Vec<u64> = Vec::new();
    let mut seen = 0u64;
    while scout.step(&mut probe) {
        let now: u64 = probe.threads.values().map(|t| t.visits.len() as u64).sum();
        if now > seen {
            seen = now;
            reached.push(scout.clock().position_ms());
        }
    }
    let mut best = (0u64, 0usize);
    let mut lo = 0usize;
    for hi in 0..reached.len() {
        while reached[lo] + window < reached[hi] {
            lo += 1;
        }
        if hi - lo + 1 > best.1 {
            best = (reached[lo], hi - lo + 1);
        }
    }
    let total = schedule.duration_ms();
    // `POLIS_SPAN=whole` sweeps the entire compressed timeline instead of the
    // busiest window. Both are worth measuring and they answer different
    // questions: the window is what the M2 recordings show, and the whole span
    // is what "does a failing session look different from a clean one" means.
    // A 78-minute compressed session in 120 frames steps 39 s per frame, well
    // inside `MARK_TTL`, so nothing expires between frames.
    let whole = std::env::var("POLIS_SPAN").as_deref() == Ok("whole");
    let (start_ms, end_ms) = if whole {
        (0, total)
    } else {
        (best.0.saturating_sub(2_000), (best.0 + window).min(total))
    };

    // The recorder's robust framing: median centre, 85th-percentile radius.
    let mut xs: Vec<f32> = Vec::new();
    let mut ys: Vec<f32> = Vec::new();
    for path in probe.files.keys() {
        if let Some(p) = position_of(&city.layout, path) {
            xs.push(p.x);
            ys.push(p.y);
        }
    }
    let focus = if xs.len() < 4 {
        None
    } else {
        xs.sort_by(f32::total_cmp);
        ys.sort_by(f32::total_cmp);
        let centre = Point {
            x: xs[xs.len() / 2],
            y: ys[ys.len() / 2],
        };
        let mut radii: Vec<f32> = xs
            .iter()
            .zip(ys.iter())
            .map(|(x, y)| (x - centre.x).abs().max((y - centre.y).abs()))
            .collect();
        radii.sort_by(f32::total_cmp);
        let extent = env_num("POLIS_EXTENT", 0.0) as f32;
        let extent = if extent > 0.0 {
            extent
        } else {
            (radii[radii.len() * 85 / 100] * 1.30).max(city.layout.extent * 0.07)
        };
        Some(Focus { centre, extent })
    };
    eprintln!(
        "  window {:.2}-{:.2} min of {:.2}; focus {:?}",
        start_ms as f64 / 60_000.0,
        end_ms as f64 / 60_000.0,
        total as f64 / 60_000.0,
        focus.map(|f| (f.centre.x, f.centre.y, f.extent))
    );

    let mut world = World::new(repo.tree().clone(), city.layout.clone());
    world.set_ubiquity(Box::new(DenylistUbiquity::default()));
    let mut driver = ReplayDriver::with_origin(schedule, Instant::now());
    let mut renderer = FrameRenderer::new(
        &city,
        FrameOptions {
            pixels,
            supersample: 2,
            focus,
            ..FrameOptions::default()
        },
    );
    let (publisher, reader) = snapshot::from_world(&world);

    let mut band: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut sat: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut agent_px = 0u64;
    let mut attention_px = 0u64;
    let mut map_px = 0u64;
    let mut marks_by_glyph: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut marks_by_outcome: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut marks_total = 0u64;
    let mut build_cost = Duration::ZERO;
    let mut now_ink: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut was_ink: BTreeMap<&'static str, u64> = BTreeMap::new();
    let frame_dt = Duration::from_millis(1000 / 24);

    for i in 0..frames {
        let f = i as f64 / (frames.max(2) - 1) as f64;
        let target = start_ms + ((end_ms - start_ms) as f64 * f) as u64;
        driver.seek(target, &mut world);
        publisher.force(&world);
        let snap = reader.load();
        let t0 = Instant::now();
        let live = renderer.build_frame(&snap, Duration::ZERO);
        build_cost += t0.elapsed();
        // `POLIS_DUMP=1` prints the mid frame's marks and agents. This is how
        // "the rosette looks like a blob" gets turned into a position and a
        // scale instead of an opinion about a screenshot.
        if std::env::var("POLIS_DUMP").is_ok() && i * 2 == frames {
            for a in &live.agents {
                eprintln!("    agent at {:?} {:?}", a.at, a.body);
            }
            for m in &live.marks {
                eprintln!(
                    "    mark at [{:.0},{:.0}] {:?} {:?} scale {:.2} count {}",
                    m.at[0], m.at[1], m.glyph, m.outcome, m.scale, m.count
                );
            }
        }
        for m in &live.marks {
            marks_total += 1;
            *marks_by_glyph
                .entry(match m.glyph {
                    Glyph::HollowCircle => "read",
                    Glyph::BarredCircle => "edit",
                    Glyph::FilledSquare => "write",
                    Glyph::FilledTriangle => "run",
                    Glyph::ConcentricCircles => "verify",
                    Glyph::Delegate => "delegate",
                })
                .or_default() += 1;
            *marks_by_outcome
                .entry(match m.outcome {
                    Outcome::Pending => "pending",
                    Outcome::Done => "done",
                    Outcome::Failed => "FAILED",
                })
                .or_default() += 1;
        }
        marks_only(&live.marks, live.unit, pixels, &mut now_ink);
        marks_only(
            &legacy_marks(&snap, renderer.view()),
            live.unit,
            pixels,
            &mut was_ink,
        );
        let canvas = renderer.render(&snap, frame_dt);
        if let Ok(dir) = std::env::var("POLIS_OUT") {
            if i % 12 == 0 {
                let dir = std::path::PathBuf::from(dir);
                std::fs::create_dir_all(&dir).expect("output directory");
                canvas
                    .write_png(&dir.join(format!("census-{i:03}.png")))
                    .expect("png");
            }
        }
        let w = canvas.width;
        for y in 0..pixels.min(canvas.height) {
            for x in 0..pixels.min(w) {
                let o = (y * w + x) * 3;
                let p = [canvas.pixels[o], canvas.pixels[o + 1], canvas.pixels[o + 2]];
                map_px += 1;
                let hi = p.iter().copied().max().unwrap_or(0);
                let lo = p.iter().copied().min().unwrap_or(0);
                if hi > 168 {
                    attention_px += 1;
                    continue;
                }
                if hi < 97 {
                    continue;
                }
                agent_px += 1;
                let name = classify(p);
                *band.entry(name).or_default() += 1;
                if f64::from(hi - lo) / f64::from(hi) >= SATURATED {
                    *sat.entry(name).or_default() += 1;
                }
            }
        }
    }

    let show = |label: &str, m: &BTreeMap<&'static str, u64>, denom: u64| {
        let mut rows: Vec<_> = m.iter().collect();
        rows.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        eprintln!("  {label} (of {denom}):");
        for (name, n) in rows {
            eprintln!(
                "    {name:<10} {n:>10}  {:>6.2}%",
                100.0 * *n as f64 / denom.max(1) as f64
            );
        }
    };
    eprintln!();
    eprintln!(
        "  {frames} frames at {pixels}px: {map_px} map px, {agent_px} in the agent band ({:.3}%), {attention_px} in the attention band ({:.3}%)",
        100.0 * agent_px as f64 / map_px.max(1) as f64,
        100.0 * attention_px as f64 / map_px.max(1) as f64,
    );
    let sat_total: u64 = sat.values().sum();
    show("agent band by ink", &band, agent_px);
    show("SATURATED agent band by ink", &sat, sat_total);
    // The channel this work changed is the *marks*, and trail and tether ink
    // swamp them by area. So the share is also reported over mark-and-agent ink
    // alone — the three outcome colours plus the two agent bodies — which is
    // the same quantity whatever the trail notation is doing.
    let ink_of = |k: &str| band.get(k).copied().unwrap_or(0);
    let mark_px: u64 = ["pending", "done", "failed", "body", "anchor"]
        .iter()
        .map(|k| ink_of(k))
        .sum();
    eprintln!(
        "  MARK INK ONLY (of {mark_px}): teal {:.2}%, red {:.2}%, neutral {:.2}%",
        100.0 * ink_of("done") as f64 / mark_px.max(1) as f64,
        100.0 * ink_of("failed") as f64 / mark_px.max(1) as f64,
        100.0 * (mark_px - ink_of("done") - ink_of("failed")) as f64 / mark_px.max(1) as f64,
    );
    eprintln!(
        "  headline: of saturated agent-band pixels, teal {:.2}%, red {:.2}%, neutral {:.2}%",
        100.0 * f64::from(u32::try_from(sat.get("done").copied().unwrap_or(0)).unwrap_or(u32::MAX))
            / sat_total.max(1) as f64,
        100.0
            * f64::from(u32::try_from(sat.get("failed").copied().unwrap_or(0)).unwrap_or(u32::MAX))
            / sat_total.max(1) as f64,
        100.0
            * (sat_total
                - sat.get("done").copied().unwrap_or(0)
                - sat.get("failed").copied().unwrap_or(0)) as f64
            / sat_total.max(1) as f64,
    );
    let share = |m: &BTreeMap<&'static str, u64>, k: &str| {
        let total: u64 = m.values().sum();
        100.0 * m.get(k).copied().unwrap_or(0) as f64 / total.max(1) as f64
    };
    eprintln!();
    eprintln!("  MARK-LAYER INK, same binary, old rule vs new:");
    for (name, m) in [("was", &was_ink), ("now", &now_ink)] {
        let total: u64 = m.values().sum();
        eprintln!(
            "    {name}: {total:>9} px  teal {:>6.2}%  red {:>6.2}%  neutral {:>6.2}%",
            share(m, "done"),
            share(m, "failed"),
            100.0
                - 100.0
                    * (m.get("done").copied().unwrap_or(0) + m.get("failed").copied().unwrap_or(0))
                        as f64
                    / total.max(1) as f64,
        );
    }
    eprintln!(
        "  LiveFrame::build cost: {:?} mean over {frames} frames (PRD §13.1 budgets 4 ms for the draw)",
        build_cost / u32::try_from(frames).unwrap_or(1)
    );
    eprintln!("  marks drawn across all frames: {marks_total}");
    show("marks by glyph", &marks_by_glyph, marks_total);
    show("marks by outcome", &marks_by_outcome, marks_total);
}
