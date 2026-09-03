//! **The deliverable measurement**: can the operator tell a failing session from
//! a clean one without reading anything?
//!
//! PRD §11.4 is the specification and it is not about hue:
//!
//! > Peripheral vision is poor at colour and good at motion onset. […] Colour
//! > alone is never the sole channel for any state.
//!
//! The independent review of M2 said the same thing with a number: *"the colour
//! channel now carries information and is correct at the mark; the map does not
//! yet shout it."* The reddest frame in 1 440 held **142 red pixels out of
//! 1.21 M** — two glyph outlines. Correct at the mark, invisible from a metre
//! away.
//!
//! So the test is a *thumbnail* test. Both sessions are rendered, both frames
//! are box-downsampled by [`THUMB`] — which is what the eye does to a screen on
//! the far side of a room, and what a box filter does to a two-pixel outline is
//! erase it — and the two thumbnails are compared on three numbers:
//!
//! | number | what it means |
//! |---|---|
//! | `peak` | the reddest single thumbnail pixel, as red-excess `r − max(g, b)` |
//! | `hot` | thumbnail pixels at or above [`HOT`] red-excess: how much of the map shouts |
//! | `mass` | mean red-excess per thumbnail pixel: total alarm, area × strength |
//!
//! Red-excess rather than "the red channel" because every ink Polis draws has a
//! red channel; only failure ink has red *dominance*. Teal `[96, 168, 150]`
//! scores 0, the neutrals score 0, and the base map — clamped at channel 48 and
//! near-grey — scores 0. So a non-zero score is failure and nothing else, at any
//! scale, which is what makes the number comparable between the two frames.
//!
//! Everything here is deterministic and hermetic: a synthetic repository, a
//! synthetic city, one thread, and two runs of the same work that differ only in
//! how eight of the forty operations went. `POLIS_OUT=<dir>` writes the frames
//! and thumbnails as PNGs.

// A measurement harness. Every line divides one count by another, and the
// geometry names itself `a`, `b`, `r`.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::items_after_statements,
    clippy::many_single_char_names
)]

use std::time::{Duration, Instant};

use polis_events::{
    Channel, Event, EventMeta, LogicalPath, OtelEvent, Outcome, Payload, SessionId, ToolCall,
    ToolKind, ToolUseId, WorkerId, WorktreeId,
};
use polis_layout::city::{self, City};
use polis_render::frame::{FrameOptions, FrameRenderer};
use polis_render::raster::Canvas;
use polis_repo::{synthetic, RepoTree};
use polis_world::{snapshot, World};

/// The box-filter factor that stands in for distance. A 600-pixel map becomes a
/// 60-pixel thumbnail: about what a second monitor across a room subtends, and
/// far enough that a two-pixel glyph outline is one part in a hundred of the
/// pixel that swallows it.
const THUMB: usize = 10;

/// Map size for the measurement, in pixels. A multiple of [`THUMB`].
const PIXELS: usize = 600;

/// Red-excess at which a thumbnail pixel counts as *shouting*.
///
/// Failure ink is `[168, 98, 92]`, a red-excess of 70 where it is solid. A
/// thumbnail pixel at 24 is therefore about a third covered by failure ink —
/// which on a near-black map is unambiguously a red pixel and not a warm grey.
const HOT: i32 = 24;

/// What one thumbnail says about failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Alarm {
    peak: i32,
    hot: usize,
    mass: i64,
    pixels: usize,
}

impl Alarm {
    fn of(thumb: &Canvas) -> Self {
        let mut peak = 0;
        let mut hot = 0;
        let mut mass = 0i64;
        for p in thumb.pixels.as_chunks::<3>().0 {
            let excess = i32::from(p[0]) - i32::from(p[1]).max(i32::from(p[2]));
            if excess > peak {
                peak = excess;
            }
            if excess >= HOT {
                hot += 1;
            }
            mass += i64::from(excess.max(0));
        }
        Self {
            peak,
            hot,
            mass,
            pixels: thumb.width * thumb.height,
        }
    }

    fn mean(self) -> f64 {
        self.mass as f64 / self.pixels as f64
    }

    fn report(self, name: &str) -> String {
        format!(
            "{name}: peak {:>3} red-excess, {:>4} hot px of {} ({:.2}% of the thumbnail), \
             mean {:.2}",
            self.peak,
            self.hot,
            self.pixels,
            100.0 * self.hot as f64 / self.pixels as f64,
            self.mean(),
        )
    }
}

fn small_city() -> City {
    city::generate_city(&synthetic::repository(320, 0x51))
}

/// One OTel `tool_result` — the channel that carries the firehose.
fn tool_result(tool: ToolKind, path: &LogicalPath, outcome: Outcome, at: Instant) -> Event {
    let mut meta = EventMeta::now(Channel::Otel).with_session(SessionId::new("s"));
    meta.observed = at;
    meta.worker = Some(WorkerId::new("agent-1"));
    let call = ToolCall {
        tool,
        tool_use_id: Some(ToolUseId::new("toolu_1")),
        paths: vec![(WorktreeId::PRIMARY, path.clone())],
        outcome,
        duration_ms: Some(4.0),
    };
    Event::new(
        meta,
        Payload::Otel(Box::new(OtelEvent::ToolResult(Box::new(call)))),
    )
}

/// Forty operations across twelve buildings. `failures` of them failed, spread
/// through the run so the newest is a few seconds old — the moment the operator
/// is being asked to read.
fn session(city: &City, failures: usize) -> World {
    let mut world = World::new(RepoTree::default(), city.layout.clone());
    let paths: Vec<LogicalPath> = city.layout.buildings.keys().take(12).cloned().collect();
    let t0 = Instant::now()
        .checked_sub(Duration::from_secs(120))
        .expect("a monotonic clock two minutes past boot");
    let mut at = t0;
    // Failures land on the last `failures` operations of a contiguous run of
    // buildings, which is what a real broken build looks like: one area of the
    // tree, several calls, all red.
    for i in 0..40usize {
        let path = &paths[i % paths.len()];
        let tool = match i % 4 {
            0 => ToolKind::Read,
            1 => ToolKind::Edit,
            2 => ToolKind::Write,
            _ => ToolKind::Bash,
        };
        let fails = failures > 0 && i >= 40 - failures * 3 && i % 3 == 1;
        let outcome = if fails {
            Outcome::Failed
        } else {
            Outcome::Done
        };
        world.apply(&tool_result(tool, path, outcome, at));
        at += Duration::from_secs(2);
    }
    world.tick(at);
    world
}

fn render(city: &City, world: &World) -> Canvas {
    let mut r = FrameRenderer::new(
        city,
        FrameOptions {
            pixels: PIXELS,
            supersample: 1,
            caption: false,
            rail: false,
            ..FrameOptions::default()
        },
    );
    let (_, reader) = snapshot::from_world(world);
    let snap = reader.load();
    // Two frames: the first settles the tweens, the second is the one measured,
    // so the number is a steady state rather than an arrival.
    r.render_owned(&snap, Duration::from_millis(40));
    let out = r.render_owned(&snap, Duration::from_millis(40));
    let built = r.build_frame(&snap, Duration::ZERO);
    let failed = built
        .marks
        .iter()
        .filter(|m| m.outcome == Outcome::Failed)
        .count();
    eprintln!(
        "  frame: unit {:.1} px, glyph r {:.1} px, {} marks ({failed} failed) -> {} alarms {:?}",
        r.unit(),
        polis_render::live::glyph_radius(built.unit, built.map_height),
        built.marks.len(),
        built.alarms.len(),
        built
            .alarms
            .iter()
            .map(|a| (a.count, a.radius.round() as i64))
            .collect::<Vec<_>>(),
    );
    out
}

fn dump(name: &str, full: &Canvas, thumb: &Canvas) {
    let Ok(dir) = std::env::var("POLIS_OUT") else {
        return;
    };
    let dir = std::path::Path::new(&dir);
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let _ = full.write_png(&dir.join(format!("{name}.png")));
    // The thumbnail is written back up to visible size with nearest-neighbour,
    // so the PNG shows exactly the pixels the measurement read.
    let mut big = Canvas::new(thumb.width * 6, thumb.height * 6, [0, 0, 0]);
    for y in 0..big.height {
        for x in 0..big.width {
            let s = ((y / 6) * thumb.width + (x / 6)) * 3;
            let d = (y * big.width + x) * 3;
            big.pixels[d..d + 3].copy_from_slice(&thumb.pixels[s..s + 3]);
        }
    }
    let _ = big.write_png(&dir.join(format!("{name}-thumb.png")));
}

/// The **M4 notation**, on the same frame: everything the live layer draws
/// except the alarm.
///
/// This is the "before" of the before/after, and it is reproduced inside the
/// same binary rather than quoted from an older build, so the two numbers cannot
/// be measuring two different cities, two different sessions or two different
/// rasterisers. The only difference is [`polis_render::salience`].
fn render_without_the_alarm(city: &City, world: &World) -> Canvas {
    let mut r = FrameRenderer::new(
        city,
        FrameOptions {
            pixels: PIXELS,
            supersample: 1,
            caption: false,
            rail: false,
            ..FrameOptions::default()
        },
    );
    let (_, reader) = snapshot::from_world(world);
    let snap = reader.load();
    r.render_owned(&snap, Duration::from_millis(40));
    let mut frame = r.build_frame(&snap, Duration::from_millis(40));
    frame.alarms.clear();
    let base = polis_render::plan::render_map_frame(city, PIXELS, 1, true, None);
    let mut canvas = base.map;
    for (i, colour) in &base.labels {
        let o = *i as usize * 3;
        if o + 3 <= canvas.pixels.len() {
            canvas.pixels[o..o + 3].copy_from_slice(colour);
        }
    }
    polis_render::live::draw(&mut canvas, &frame, polis_render::live::TrailStyle::Timed);
    canvas
}

/// The acceptance test, stated as PRD §11.4 states it: **at a glance**.
///
/// Two sessions, identical but for eight failed operations out of forty. Both
/// reduced to a 60×60 thumbnail. The clean one must be silent in the alarm
/// channel and the failing one must not be.
#[test]
fn a_failing_session_and_a_clean_one_are_different_at_thumbnail_size() {
    let city = small_city();
    let clean = render(&city, &session(&city, 0));
    let failing = render(&city, &session(&city, 8));
    let before = render_without_the_alarm(&city, &session(&city, 8));

    let clean_thumb = clean.downsample(THUMB);
    let failing_thumb = failing.downsample(THUMB);
    let before_thumb = before.downsample(THUMB);
    dump("clean", &clean, &clean_thumb);
    dump("failing", &failing, &failing_thumb);
    dump("failing-m4", &before, &before_thumb);

    let a = Alarm::of(&clean_thumb);
    let b = Alarm::of(&failing_thumb);
    let m4 = Alarm::of(&before_thumb);
    eprintln!("{}", a.report("clean       "));
    eprintln!("{}", m4.report("failing (M4)"));
    eprintln!("{}", b.report("failing (M5)"));
    eprintln!(
        "  clean -> failing:  peak x{:.1}, hot +{}, mass x{:.1}",
        f64::from(b.peak) / f64::from(a.peak.max(1)),
        b.hot as i64 - a.hot as i64,
        b.mean() / a.mean().max(0.001),
    );
    eprintln!(
        "  M4 -> M5 on the same frame:  hot {} -> {} (x{}), peak {} -> {}",
        m4.hot,
        b.hot,
        b.hot / m4.hot.max(1),
        m4.peak,
        b.peak,
    );
    // The defect, reproduced: with the alarm suppressed, a failing session and a
    // clean one differ by a couple of thumbnail pixels.
    assert!(
        m4.hot <= 4,
        "the M4 baseline is not the one the review measured: {}",
        m4.report("failing (M4)")
    );
    assert!(
        b.hot >= m4.hot * 8,
        "the alarm did not move the number it exists to move: {} -> {}",
        m4.hot,
        b.hot
    );

    assert_eq!(
        a.hot, 0,
        "a clean session shouts: {} thumbnail pixels are red",
        a.hot
    );
    assert!(
        a.peak < HOT,
        "a clean session has a red pixel at all: {}",
        a.report("clean")
    );
    // Forty hot thumbnail pixels is a coherent red ring, not a speck: it is 1 %
    // of a 60 × 60 thumbnail, contiguous, and the only saturated thing on a map
    // whose every other layer is a neutral or a teal.
    assert!(
        b.hot >= 40,
        "the failing session is not visible at thumbnail size: {}",
        b.report("failing")
    );
    // …and it must be *red*, not a warm smear. A peak near failure ink's own 70
    // means at least one thumbnail pixel is essentially solid alarm.
    assert!(
        b.peak >= 50,
        "no thumbnail pixel is properly red: {}",
        b.report("failing")
    );
    assert!(
        b.hot >= a.hot + 40 && b.mean() > a.mean() * 1.8,
        "the two thumbnails are too close to tell apart:\n  {}\n  {}",
        a.report("clean  "),
        b.report("failing"),
    );
}

/// The salience budget, which is the other half of the acceptance test: PRD §17
/// forbids "a beautiful swarm view that makes the operator feel informed while
/// telling them nothing actionable", and a map that is 30 % red says only that
/// Polis panics.
#[test]
fn the_alarm_does_not_swallow_the_map() {
    let city = small_city();
    let failing = render(&city, &session(&city, 8));
    let thumb = failing.downsample(THUMB);
    let b = Alarm::of(&thumb);
    let share = b.hot as f64 / b.pixels as f64;
    eprintln!("{}", b.report("failing"));
    assert!(
        share < 0.20,
        "the alarm inked {:.1}% of the thumbnail; that is fog, not a signal",
        share * 100.0
    );
}

/// The same measurement on the operator's **own** sessions, where the defect was
/// found.
///
/// Replays a real transcript, walks the whole compressed timeline, and reports
/// the reddest frame under both notations — M4's (the alarm suppressed) and
/// M5's. The live layer is drawn on a neutral ground rather than on the city,
/// because the two notations are being compared with each other and the base map
/// is identical under both; it contributes no red excess at all (measured: the
/// clean city's peak is 15 and its hot count is 0).
///
/// ```sh
/// POLIS_SESSION=29c2fc6f cargo test --release -p polis-render \
///     --test salience -- --ignored --nocapture
/// ```
#[test]
#[ignore = "reads the operator's real sessions"]
fn the_reddest_frame_of_a_real_session_under_both_notations() {
    let Some(projects) = polis_world::sessions::default_projects_dir().filter(|p| p.is_dir())
    else {
        eprintln!("skipped: no ~/.claude/projects on this machine");
        return;
    };
    let index = polis_world::sessions::SessionIndex::scan_with(
        &projects,
        &polis_world::sessions::IndexOptions::quick(),
    )
    .expect("scan");
    let want = std::env::var("POLIS_SESSION").unwrap_or_else(|_| "29c2fc6f".to_owned());
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
    let mapper = polis_events::PathMapper::new(&repo_path).expect("mapper");
    let mut schedule = sample.schedule(&mapper).expect("read");
    schedule.compress_idle_gaps(Some(polis_world::replay::DEFAULT_IDLE_GAP_CAP));

    let mut world = World::new(repo.tree().clone(), city.layout.clone());
    world.set_ubiquity(Box::new(polis_world::DenylistUbiquity::default()));
    let origin = Instant::now();
    let mut driver = polis_world::replay::ReplayDriver::with_origin(schedule.clone(), origin);
    let mut r = FrameRenderer::new(
        &city,
        FrameOptions {
            pixels: PIXELS,
            supersample: 1,
            caption: false,
            rail: false,
            ..FrameOptions::default()
        },
    );
    let (publisher, reader) = snapshot::from_world(&world);

    const FRAMES: usize = 120;
    let span = schedule.duration_ms().max(1);
    let mut best = (
        0usize,
        Alarm::of(&Canvas::new(1, 1, [0, 0, 0])),
        0usize,
        0usize,
    );
    let mut best_m4 = Alarm::of(&Canvas::new(1, 1, [0, 0, 0]));
    let mut frames_with_an_alarm = 0usize;
    for i in 0..FRAMES {
        driver.seek(span * i as u64 / (FRAMES as u64 - 1), &mut world);
        publisher.force(&world);
        let snap = reader.load();
        let frame = r.build_frame(&snap, Duration::from_millis(40));
        let failed = frame
            .marks
            .iter()
            .filter(|m| m.outcome == Outcome::Failed)
            .count();
        if !frame.alarms.is_empty() {
            frames_with_an_alarm += 1;
        }
        let mut with = Canvas::new(PIXELS, PIXELS, [18, 19, 22]);
        polis_render::live::draw(&mut with, &frame, polis_render::live::TrailStyle::Timed);
        let a = Alarm::of(&with.downsample(THUMB));

        let mut without = Canvas::new(PIXELS, PIXELS, [18, 19, 22]);
        let mut bare = frame.clone();
        bare.alarms.clear();
        polis_render::live::draw(&mut without, &bare, polis_render::live::TrailStyle::Timed);
        let b = Alarm::of(&without.downsample(THUMB));

        if a.hot > best.1.hot {
            best = (i, a, failed, frame.alarms.len());
            best_m4 = b;
        }
    }
    eprintln!(
        "session {} — {} events, {FRAMES} frames, {frames_with_an_alarm} with an alarm",
        sample.session,
        schedule.len(),
    );
    eprintln!(
        "  reddest frame #{} ({} failed marks, {} alarms)",
        best.0, best.2, best.3
    );
    eprintln!("  {}", best_m4.report("M4 (no alarm)"));
    eprintln!("  {}", best.1.report("M5 (alarm)   "));
    assert!(
        best.1.hot > best_m4.hot,
        "the alarm made no difference on real data"
    );
}

/// Writes the deliverable: three renders of the same city, and the three
/// thumbnails underneath them.
///
/// | column | what it is |
/// |---|---|
/// | clean | forty operations, none failed |
/// | failing, M4 | the same forty with eight failures, drawn the way M4 drew them |
/// | failing, M5 | identical world, identical frame, with the alarm |
///
/// The bottom row is the whole argument: it is what the top row looks like from
/// the far side of a room, and only one of the three thumbnails is telling the
/// operator anything.
///
/// ```sh
/// cargo test --release -p polis-render --test salience -- --ignored sheet --nocapture
/// ```
#[test]
#[ignore = "writes images"]
fn writes_the_before_and_after_sheet() {
    let city = small_city();
    let cols = [
        ("CLEAN", render(&city, &session(&city, 0))),
        (
            "FAILING - M4 NOTATION",
            render_without_the_alarm(&city, &session(&city, 8)),
        ),
        (
            "FAILING - M5, WITH THE ALARM",
            render(&city, &session(&city, 8)),
        ),
    ];
    let scale = 6;
    let thumb_px = PIXELS / THUMB * scale;
    let pad = 24;
    let caption = 34;
    let width = PIXELS * cols.len() + pad * (cols.len() + 1);
    let height = caption + PIXELS + caption + thumb_px + pad * 2;
    let mut sheet = Canvas::new(width, height, [12, 13, 15]);
    for (i, (name, canvas)) in cols.iter().enumerate() {
        let x0 = pad + i * (PIXELS + pad);
        sheet.text(x0 as f64, 12.0, name, 2.0, [150, 156, 168]);
        for y in 0..PIXELS {
            let s = y * PIXELS * 3;
            let d = ((y + caption) * width + x0) * 3;
            sheet.pixels[d..d + PIXELS * 3].copy_from_slice(&canvas.pixels[s..s + PIXELS * 3]);
        }
        let a = Alarm::of(&canvas.downsample(THUMB));
        sheet.text(
            x0 as f64,
            (caption + PIXELS + 10) as f64,
            &format!("{} HOT THUMBNAIL PX / 3600", a.hot),
            2.0,
            [150, 156, 168],
        );
        // Nearest-neighbour back up to visible size: the pixels on the sheet are
        // exactly the pixels the measurement read.
        let thumb = canvas.downsample(THUMB);
        let tx = x0 + (PIXELS - thumb_px) / 2;
        let ty = caption + PIXELS + caption;
        for y in 0..thumb_px {
            for x in 0..thumb_px {
                let s = ((y / scale) * thumb.width + (x / scale)) * 3;
                let d = ((ty + y) * width + tx + x) * 3;
                sheet.pixels[d..d + 3].copy_from_slice(&thumb.pixels[s..s + 3]);
            }
        }
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("docs")
        .join("attention");
    std::fs::create_dir_all(&dir).expect("docs/attention");
    let out = dir.join("salience-before-after.png");
    sheet.write_png(&out).expect("write the sheet");
    eprintln!("wrote {}", out.display());
}
