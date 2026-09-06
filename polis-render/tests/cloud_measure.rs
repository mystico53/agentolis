//! Measurement harness for layer 3 — the territory clouds (PRD §6.4, §10.4).
//!
//! `plan`'s `a_cloud_is_a_contour_and_a_hatch_and_not_an_area_fill` runs the
//! fill test on a **synthetic** validation image, in CI, on every commit. This
//! one runs it on the operator's **real** sessions, several replayed at once
//! into one world so the map carries more than one territory — the only
//! arrangement in which the two claims that matter can be tested at all:
//!
//! * *"Multi-lobed shapes come free"* needs a territory whose observations are
//!   in two places, and a synthetic ring of kernels is one place by construction.
//! * *"Overlap is field addition … which is exactly the contention signal"*
//!   needs two territories, and a replay of one session has one.
//!
//! ```text
//! POLIS_REPO=qurio-toolset POLIS_THREADS=3 \
//!   cargo test -p polis-render --release --test cloud_measure -- --ignored --nocapture
//! ```
//!
//! | env | meaning |
//! |---|---|
//! | `POLIS_REPO` | substring of the repository path; default `qurio-toolset` |
//! | `POLIS_THREADS` | how many sessions to overlay as concurrent threads; default 3 |
//! | `POLIS_FRAMES` / `POLIS_PIXELS` | frame count and map size |
//! | `POLIS_FROM` / `POLIS_TO` | fraction of the overlaid timeline to measure |
//! | `POLIS_SESSION` | one session by id prefix, for the two camera tests |
//! | `POLIS_OUT` | directory to write the measured frames into, if wanted |
//!
//! Three tests, and only the first one asserts:
//!
//! * [`measure_clouds_on_real_threads`] — the measurement.
//! * [`gif_versus_window_is_a_difference_of_scale`] — why the recorded GIFs have
//!   clouds the operator likes and the live window has none. It writes
//!   `camera-scale.png`.
//! * [`lift_versus_shipped_side_by_side`] — the picture of the removed lift
//!   beside the picture the product draws. It writes `lift-versus-shipped.png`.
//!
//! # The measurement is **paired**, and it has to be
//!
//! The obvious under-cloud test compares base-map pixels beneath a cloud with
//! base-map pixels elsewhere in the same image. On the synthetic city that is
//! sound. On a real one it is not: a cloud sits where the work is, the work is
//! in the source tree, and the source tree is a denser and brighter part of the
//! map than the vendored corners a cloud never reaches. Measured that way this
//! layer reported a 14-level "gap" that was entirely the city's own geography.
//!
//! So every frame is rendered **twice** — once as the renderer draws it, once
//! with the cloud layer painted on top — and the two are compared *pixel for
//! pixel over the same mask*. Whatever moves is the cloud's doing and nothing
//! else is.
//!
//! # The lift, and why it is gone
//!
//! This harness used to re-implement the shipped ranking and cap **with PRD
//! §6.2's convergence gate lifted**, and said so in this header. The reason was
//! real: no session in `~/.claude/projects` produced a converged territory,
//! because §6.2 needs the weight-trimmed lowest common ancestor at `depth >= 2`
//! and a session that works across a repository has the repository as its
//! ancestor at `depth 0`. Every measurement below was therefore taken on a
//! cloud `polis_render::frame` would not have drawn — the harness flattering a
//! layer that, live, drew nothing at all, which is exactly the gap the operator
//! saw between the recorded GIFs and their own map.
//!
//! PRD §6.4's lobes closed it. The depth-and-mass test now runs per cluster, so
//! an orchestrator spread across the tree gets several lobes where a single
//! ancestor got nothing, and `territory::select_clouds` admits a territory with
//! lobes **or** a claim. So the lift is removed and there is nothing left in
//! here that decides which territory gets a cloud:
//!
//! * the kernels come from [`FrameRenderer`]'s own `build`, through the shipped
//!   `select_clouds`, the shipped cap and the shipped camera;
//! * the field measured is [`FrameRenderer::cloud_field`] — the tween's own
//!   output, after this renderer drew it, not a second sample of the same
//!   kernels;
//! * the counts printed are [`FrameRenderer::cloud_census`], which is
//!   `select_clouds`'s own verdict on every thread.
//!
//! `RESULT` therefore carries `shown/unplaced/dormant/capped`. If a real session
//! stops producing clouds, this test fails with the reason in the line above the
//! failure, and the fix is in `polis_world::territory`, never here.
//!
//! # It measures the shipped path **at its best moment**, and that is a limit
//!
//! Read the two claims below carefully: `shown_frames > 0` and
//! `cloud_frames > 0`. They say the shipped selection can put a cloud on the map
//! — not how often it does. And the frames they are evaluated over are chosen by
//! `polis_render::pacing::plan`, which weights frames by **event mass**, so the
//! sample lands where the work is dense by construction.
//!
//! The gap that costs is real and was measured while fixing the operator's
//! *"i dont see any clouds"*: event-weighted sampling of the same sessions gives
//! **94 %** cloud coverage where sampling the same replays on a fixed
//! session-time grid gives **62 %**. A layer that draws a cloud in every busy
//! minute and nothing in between is exactly what an operator watching a live map
//! experiences as empty, and every assertion in this file stays green through it.
//!
//! So: green here means *the notation is right and the selection can fire*. It
//! does not mean the map has clouds on it, and no test in this workspace
//! currently asserts that it does — the wall-clock-sampled coverage harness that
//! would is not written. `POLIS_FROM`/`POLIS_TO` are the nearest thing available,
//! and the table below is what happens when the window moves.
//!
//! # Which window, and why the number moves
//!
//! With the lift gone the harness measures whatever window it is pointed at,
//! and the two available windows do not agree. Both are recorded here because
//! the difference is a finding and not noise. Three sessions of `qurio-toolset`,
//! shipped path, 64 frames:
//!
//! | window | shown/frame | contested | ink | worst show-through |
//! |---|---|---|---|---|
//! | `POLIS_FROM=0 POLIS_TO=0.1` — the concurrent one | 0.86 | 458 px | 27 % | **66 %** |
//! | `POLIS_FROM=0 POLIS_TO=0.3` | 0.92 | 0 | 28 % | **64 %** |
//! | default, `pacing::plan`'s derived window | 0.80 | 0 | 39 % | **47 %** |
//!
//! The default is the odd one out and the reason is printed on its own pacing
//! line: `plan` lands its 64 frames in the **last minute** of a 134-minute
//! overlaid timeline, where one thread is stacking 35 kernels on one district
//! and no second territory is alive at all — `contested_max=0`, which is the
//! arrangement this file's own header says it exists to avoid.
//!
//! In that one-thread burst the iso bands come out as thin annuli, and
//! [`polis_render::live::CLOUD_CONTOUR_FRACTION`]'s promise — *"a contour is at
//! most a tenth of its own band's characteristic radius"* — cannot be kept,
//! because `contour_steps` clamps its step **up** to one pixel and a band three
//! pixels thick has no room for one. `BANDS` prints the consequence directly:
//! the body band inks 76 % of itself on the worst frame while its hatch is
//! designed for 14 %. That is a real defect in the notation at small sizes, it
//! is the same root cause as the operator's missing clouds — see
//! `gif_versus_window_is_a_difference_of_scale` — and it is recorded here rather
//! than tuned away.

// A measurement harness: counts, ratios, and one long linear script whose order
// is part of what it is measuring.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use polis_events::PathMapper;
use polis_render::frame::{FrameOptions, FrameRenderer};
use polis_render::live::{
    self, BandMap, CloudField, CloudKernel, CloudTween, CLOUD_TONES, NO_BAND,
};
use polis_render::plan::Focus;
use polis_render::plan::BASE_MAP_CEILING;
use polis_render::raster::Canvas;
use polis_world::replay::{ReplayDriver, ReplaySchedule};
use polis_world::sessions::{IndexOptions, SessionIndex};
use polis_world::{snapshot, DenylistUbiquity, World};

/// Rec. 709 luma, the measure `plan`'s palette is written in.
fn luma(p: [u8; 3]) -> f64 {
    0.2126f64.mul_add(
        f64::from(p[0]),
        0.7152f64.mul_add(f64::from(p[1]), 0.0722 * f64::from(p[2])),
    )
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[(((sorted.len() - 1) as f64) * q).round() as usize]
}

fn pixel(canvas: &Canvas, x: usize, y: usize) -> [u8; 3] {
    let i = (y * canvas.width + x) * 3;
    [canvas.pixels[i], canvas.pixels[i + 1], canvas.pixels[i + 2]]
}

/// What one frame's cloud layer did to the image under it.
struct CloudStats {
    /// Pixels painted in a cloud tone.
    inked: usize,
    /// Pixels inside the cloud's dilated footprint.
    footprint: usize,
    /// Distinct iso levels present.
    levels: usize,
    /// Median luminance of the footprint's base-map pixels **before** the cloud.
    before: f64,
    /// The same pixels **after** it.
    after: f64,
    /// Mean of the same, which moves before a median does.
    before_mean: f64,
    /// Mean after.
    after_mean: f64,
    /// Connected components of the footprint — PRD §6.4's lobes.
    lobes: usize,
    /// Pixels the city still shows through that the cloud nevertheless changed.
    /// Zero is the claim: the marks are opaque and nothing else is touched.
    disturbed: usize,
    /// Median luminance of the city **outside** every cloud, for reference. On a
    /// real map this differs from `after` by the city's own geography and not by
    /// anything the cloud did — see the module docs.
    elsewhere: f64,
    /// Per iso band, fringe → core: `(footprint, inked)`.
    ///
    /// The overall ink share is an average over three notations with three
    /// different designed densities — a 14 px hatch at the fringe and a 6 px one
    /// at the core — so the average alone cannot say whether a cloud is too inky
    /// or merely mostly core. This can.
    by_band: [(usize, usize); 3],
}

/// Measures what the cloud layer did, comparing one frame with the same frame
/// drawn without it.
///
/// The cloud mask is taken from **exact tone equality**, which is the whole
/// reason `live` paints opaque marks: a blended wash has no exact tone and could
/// not be masked this way at all.
fn measure(bare: &Canvas, with: &Canvas, bands: Option<&BandMap>, map_rows: usize) -> CloudStats {
    let w = with.width;
    let rows = map_rows.min(with.height);
    let mut cloud = vec![false; rows * w];
    let mut levels: BTreeSet<[u8; 3]> = BTreeSet::new();
    for y in 0..rows {
        for x in 0..w {
            let p = pixel(with, x, y);
            if CLOUD_TONES.contains(&p) && pixel(bare, x, y) != p {
                cloud[y * w + x] = true;
                levels.insert(p);
            }
        }
    }
    // The cloud's footprint is the **band region itself** — every pixel the
    // field put inside an iso level — and not a dilation of the marks.
    //
    // Dilating the ink was the first definition and it is circular: a cloud
    // drawn as a thin contour has a footprint that hugs its own stroke, so the
    // inked share saturates near a half however sparse the notation is, and the
    // number stops measuring fog and starts measuring stroke width. The band
    // region is what the field claims, which is what "under a cloud" means.
    let k = 8usize;
    let (cw, ch) = (w.div_ceil(k), rows.div_ceil(k));
    let mut cell = vec![false; cw * ch];
    let mut covered = vec![false; rows * w];
    match bands {
        Some(b) => {
            for y in 0..rows {
                for x in 0..w {
                    if b.at(x, y) != NO_BAND {
                        covered[y * w + x] = true;
                        cell[(y / k) * cw + x / k] = true;
                    }
                }
            }
        }
        None => {
            for y in 0..rows {
                for x in 0..w {
                    if cloud[y * w + x] {
                        covered[y * w + x] = true;
                        cell[(y / k) * cw + x / k] = true;
                    }
                }
            }
        }
    }
    // Lobes: 4-connected components of the dilated footprint. PRD §6.4 promises
    // "an agent working in auth with one worker in tests gets two lobes and a
    // thin connecting band", and a blob is the failure it names.
    let mut seen = vec![false; cw * ch];
    let mut lobes = 0usize;
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..cw * ch {
        if !cell[start] || seen[start] {
            continue;
        }
        lobes += 1;
        stack.push(start);
        seen[start] = true;
        while let Some(i) = stack.pop() {
            let (x, y) = (i % cw, i / cw);
            let mut neighbours = [None; 4];
            if x > 0 {
                neighbours[0] = Some(y * cw + x - 1);
            }
            if x + 1 < cw {
                neighbours[1] = Some(y * cw + x + 1);
            }
            if y > 0 {
                neighbours[2] = Some((y - 1) * cw + x);
            }
            if y + 1 < ch {
                neighbours[3] = Some((y + 1) * cw + x);
            }
            for j in neighbours.into_iter().flatten() {
                if cell[j] && !seen[j] {
                    seen[j] = true;
                    stack.push(j);
                }
            }
        }
    }

    let mut inked = 0usize;
    let mut footprint = 0usize;
    let mut disturbed = 0usize;
    let mut by_band = [(0usize, 0usize); 3];
    let mut before: Vec<f64> = Vec::new();
    let mut after: Vec<f64> = Vec::new();
    let mut elsewhere: Vec<f64> = Vec::new();
    for y in 0..rows {
        for x in 0..w {
            let under = covered[y * w + x];
            if under {
                footprint += 1;
                let is_ink = cloud[y * w + x];
                if is_ink {
                    inked += 1;
                }
                if let Some(b) = bands {
                    let level = b.at(x, y);
                    if let Some(slot) = by_band.get_mut(level as usize) {
                        slot.0 += 1;
                        slot.1 += usize::from(is_ink);
                    }
                }
            }
            // The city showing *through*: pixels that are still base map in the
            // finished image. This is the population the claim is about — "a set
            // of marks the city shows through, not an area fill" — and it is the
            // one an area fill destroys, because a fill leaves none of it.
            let f = pixel(with, x, y);
            if f.iter().copied().max().unwrap_or(0) > BASE_MAP_CEILING {
                continue;
            }
            // Only *city* pixels: open sea sits at L 4 and clouds do not fall on
            // it evenly, so including it would compare a coastline with a
            // downtown and call the difference fog.
            let lf = luma(f);
            if lf <= 12.0 {
                continue;
            }
            if under {
                before.push(luma(pixel(bare, x, y)));
                after.push(lf);
                if pixel(bare, x, y) != f {
                    disturbed += 1;
                }
            } else {
                elsewhere.push(lf);
            }
        }
    }
    elsewhere.sort_by(f64::total_cmp);
    let mean = |v: &[f64]| {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    let (before_mean, after_mean) = (mean(&before), mean(&after));
    before.sort_by(f64::total_cmp);
    after.sort_by(f64::total_cmp);
    CloudStats {
        inked,
        footprint,
        levels: levels.len(),
        before: percentile(&before, 0.5),
        after: percentile(&after, 0.5),
        before_mean,
        after_mean,
        lobes,
        disturbed,
        elsewhere: percentile(&elsewhere, 0.5),
        by_band,
    }
}

/// The overlaid world every measurement in this file runs on: the `N` largest
/// real sessions of one repository, each shifted to start at zero so sessions
/// that ran days apart become threads running side by side.
///
/// Their **paths** are real and so is everything the territory inference does
/// with them; only the start times move, and they move to produce the one
/// arrangement PRD §6.4 is about — several territories on one map at one
/// moment.
fn overlaid_world() -> Option<(
    polis_layout::city::City,
    polis_repo::tree::RepoIndex,
    ReplaySchedule,
    String,
)> {
    let projects = polis_world::sessions::default_projects_dir().filter(|p| p.is_dir());
    let Some(projects) = projects else {
        eprintln!("skipped: no ~/.claude/projects on this machine");
        return None;
    };
    let index = SessionIndex::scan_with(&projects, &IndexOptions::quick()).expect("scan");
    let want = std::env::var("POLIS_REPO").unwrap_or_else(|_| "qurio-toolset".to_owned());
    let threads: usize = std::env::var("POLIS_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let mut candidates: Vec<_> = index
        .sessions
        .iter()
        .filter(|s| s.repo_exists && s.repo.is_some())
        .filter(|s| {
            s.repo
                .as_ref()
                .is_some_and(|r| r.to_string_lossy().contains(&want))
        })
        .collect();
    candidates.sort_by_key(|s| std::cmp::Reverse(s.bytes));
    candidates.truncate(threads);
    if candidates.is_empty() {
        eprintln!("skipped: no session under a repository matching {want}");
        return None;
    }
    let repo_path = candidates[0].repo.clone().expect("a repo");
    let repo = polis_repo::tree::RepoIndex::open(&repo_path).expect("index");
    let city = polis_layout::city::generate_city(repo.tree());
    let mapper = PathMapper::new(&repo_path).expect("mapper");

    let mut merged: Vec<polis_events::RecordedEvent> = Vec::new();
    let mut header = None;
    let mut spans: Vec<(String, u64, usize)> = Vec::new();
    for sample in &candidates {
        let schedule = sample.schedule(&mapper).expect("offline full read");
        let base = schedule
            .entries
            .first()
            .map_or(0, polis_world::replay::ScheduledEvent::session_ms);
        let n = schedule.entries.len();
        let mut last = 0;
        for entry in &schedule.entries {
            let mut e = entry.event.clone();
            e.monotonic_offset_ms = e.monotonic_offset_ms.saturating_sub(base);
            last = last.max(e.monotonic_offset_ms);
            merged.push(e);
        }
        spans.push((sample.session.to_string(), last, n));
        if header.is_none() {
            header = Some(schedule.header.clone());
        }
    }
    let header = header.expect("a header");
    let mut schedule = ReplaySchedule::from_events(
        header,
        merged,
        polis_ingest::transcript::ParseStats::default(),
        Vec::new(),
    );
    // Compressed *after* the merge: the gap that matters is the gap in the
    // overlaid timeline, and three sessions interleaved have far fewer idle
    // stretches than any one of them alone.
    schedule.compress_idle_gaps(Some(polis_world::replay::DEFAULT_IDLE_GAP_CAP));
    let mut label = format!(
        "{} threads over {} — {} events, {:.1} min overlaid",
        spans.len(),
        repo_path.display(),
        schedule.len(),
        schedule.duration_ms() as f64 / 60_000.0,
    );
    for (id, last, n) in &spans {
        use std::fmt::Write as _;
        let _ = write!(
            label,
            "; {id}: {n} events, {:.1} min",
            *last as f64 / 60_000.0
        );
    }
    Some((city, repo, schedule, label))
}

/// The camera `docs/replay`'s recorder points at a session, reproduced exactly.
///
/// A pre-pass over the whole schedule, the median of the touched paths for the
/// centre, the 85th-percentile radius times 1.30 for the extent, floored at 7 %
/// of the city. Copied from `frame`'s `replay_real_session` rather than
/// re-invented, because the question these tests ask is *what the recorder did
/// that the window does not*, and a camera invented here would answer a
/// different one.
///
/// **This function is the answer to that question.** Everything else about the
/// cloud layer is identical between the two paths; this is the only difference,
/// and it is worth between four and eight times the scale.
fn recorder_focus(city: &polis_layout::city::City, schedule: &ReplaySchedule) -> Option<Focus> {
    let mut scout = World::for_replay(city.layout.clone());
    ReplayDriver::with_origin(schedule.clone(), Instant::now()).run_to_end(&mut scout);
    let mut xs: Vec<f32> = Vec::new();
    let mut ys: Vec<f32> = Vec::new();
    for path in scout.files.keys() {
        if let Some(p) = polis_render::frame::position_of(&city.layout, path) {
            xs.push(p.x);
            ys.push(p.y);
        }
    }
    if xs.len() < 4 {
        return None;
    }
    xs.sort_by(f32::total_cmp);
    ys.sort_by(f32::total_cmp);
    let centre = polis_layout::Point {
        x: xs[xs.len() / 2],
        y: ys[ys.len() / 2],
    };
    let mut radii: Vec<f32> = xs
        .iter()
        .zip(ys.iter())
        .map(|(x, y)| (x - centre.x).abs().max((y - centre.y).abs()))
        .collect();
    radii.sort_by(f32::total_cmp);
    let extent = (radii[radii.len() * 85 / 100] * 1.30).max(city.layout.extent * 0.07);
    Some(Focus { centre, extent })
}

/// Overlays several real sessions of one repository into one world and measures
/// the cloud layer frame by frame.
#[test]
#[ignore = "reads the operator's real sessions"]
fn measure_clouds_on_real_threads() {
    let frames: usize = std::env::var("POLIS_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64);
    let pixels: usize = std::env::var("POLIS_PIXELS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(900);
    let Some((city, repo, schedule, label)) = overlaid_world() else {
        return;
    };
    eprintln!("{label}");

    let mut world = World::new(repo.tree().clone(), city.layout.clone());
    world.set_ubiquity(Box::new(DenylistUbiquity::default()));
    let mut driver = ReplayDriver::with_origin(schedule.clone(), Instant::now());
    let opts = FrameOptions {
        pixels,
        supersample: 1,
        caption: false,
        rail: false,
        ..FrameOptions::default()
    };
    // Two renderers over one world. `bare` has the cloud layer switched off at
    // the cap, so the paired comparison sees the cloud layer and nothing else;
    // `shipped` is what the application draws, cloud gate and all, and is what
    // gets written out to be looked at.
    let mut renderer = FrameRenderer::new(
        &city,
        FrameOptions {
            cloud_cap: 0,
            ..opts
        },
    );
    let mut shipped = FrameRenderer::new(&city, opts);
    let (publisher, reader) = snapshot::from_world(&world);

    // `pacing` derives the frame step from the schedule's own event density,
    // which matters more here than anywhere else: three overlaid sessions span
    // days, PRD §6.3 decays a kernel with a 90 s half-life, and frames spread
    // evenly over the whole span land almost entirely in the idle between them.
    //
    // `POLIS_FROM`/`POLIS_TO` override it with a fraction of the overlaid
    // timeline, which is how the *concurrent* window is reached: every session
    // was shifted to start at zero, so the threads are all alive near the
    // beginning and the densest window is usually one session's own burst.
    let plan = polis_render::pacing::plan(&schedule, frames);
    let span = |k: &str, d: f64| -> f64 {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(d)
            .clamp(0.0, 1.0)
    };
    let (from, to) = (span("POLIS_FROM", 0.0), span("POLIS_TO", 1.0));
    let windowed = std::env::var("POLIS_FROM").is_ok() || std::env::var("POLIS_TO").is_ok();
    let placements: Vec<u64> = if windowed {
        let total = schedule.duration_ms() as f64;
        let (a, b) = ((total * from) as u64, (total * to.max(from)) as u64);
        (0..frames)
            .map(|i| a + (b - a) * i as u64 / (frames.max(2) as u64 - 1))
            .collect()
    } else {
        plan.frames_ms.clone()
    };
    eprintln!(
        "pacing[{}]: {} frames over {:.2}-{:.2} min, {} live events inside — {}",
        if windowed { "window" } else { "derived" },
        placements.len(),
        placements[0] as f64 / 60_000.0,
        placements[placements.len() - 1] as f64 / 60_000.0,
        plan.live_events,
        plan.verdict(),
    );

    let out = std::env::var("POLIS_OUT")
        .ok()
        .map(std::path::PathBuf::from);
    if let Some(dir) = &out {
        std::fs::create_dir_all(dir).expect("output directory");
    }

    let mut cloud_frames = 0usize;
    let mut shown_frames = 0usize;
    let mut lobe_max = 0usize;
    let mut crowd_max = 0usize;
    let mut ink_share_max = 0usize;
    let mut worst_median = 0.0f64;
    let mut worst_mean = 0.0f64;
    let mut worst_regional = 0.0f64;
    let mut disturbed_total = 0usize;
    let mut show_through_min = 100usize;
    let mut inked_total = 0usize;
    let mut footprint_total = 0usize;
    let mut shipped_cloud_px = 0usize;
    let mut levels_seen = 0usize;
    let mut widest_max = 0.0f64;
    let mut band_totals = [(0usize, 0usize); 3];
    let mut worst_frame = (0usize, 0usize, 0usize, [(0usize, 0usize); 3]);
    let mut census_total = live::CloudCensus::default();
    let mut census_frames = 0usize;
    let mut prev_cloud: Option<usize> = None;
    let mut jumps: Vec<f64> = Vec::new();
    let mut draw: Vec<Duration> = Vec::new();
    let dt = Duration::from_millis(1000 / 24);
    for (i, target) in placements.iter().copied().enumerate() {
        driver.seek(target, &mut world);
        publisher.force(&world);
        let snap = reader.load();
        // Both renderers see the same snapshot and the same `dt`, so every
        // tween in them — agents, marks, the rise of a building — advances
        // identically. The only thing that differs is the cap, so the only
        // thing that can differ in the two images is the cloud layer.
        let bare = renderer.render_owned(&snap, dt);
        let shipped_frame = shipped.render_owned(&snap, dt);
        let shipped_px = shipped_frame
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| CLOUD_TONES.contains(p))
            .count();
        shipped_cloud_px = shipped_cloud_px.max(shipped_px);

        // PRD §10.4's selection, as the shipped renderer decided it. Nothing in
        // this file ranks, caps or gates a territory any more.
        let census = shipped.cloud_census();
        shown_frames += usize::from(census.shown > 0);
        widest_max = widest_max.max(census.widest_px);
        census_total.shown += census.shown;
        census_total.kernels += census.kernels;
        census_total.unplaced += census.unplaced;
        census_total.dormant += census.dormant;
        census_total.capped += census.capped;
        census_frames += 1;

        // The paired image: the field the shipped renderer just drew, painted
        // over the same frame drawn without it. The field is *not* re-sampled
        // here — `cloud_field` is the tween's own output — so the mask is the
        // one the product put on the screen.
        //
        // Painted as a **stack**, which is the shipped notation: one banded,
        // contoured and hatched layer per territory, over each other. Measuring
        // the summed single map instead would measure a picture the product
        // stopped drawing, and would miss the one thing worth measuring about a
        // stack — that `N` layers over one place still do not fog the city
        // under them.
        //
        // With no tints, so every layer comes out in the neutral `CLOUD_TONES`
        // and `measure`'s mask can stay exact tone equality. The geometry is
        // identical either way: `stack` keys the hue on the tint and the axis on
        // the tint's slot, falling back to the layer's rank, so an untinted
        // stack is the shipped stack in greyscale.
        let mut with = bare.clone();
        let started = Instant::now();
        let mut contested = 0usize;
        let mut band_map = None;
        let mut peak = 0.0f32;
        if let Some(field) = shipped.cloud_field() {
            peak = field.peak();
            let stack = field.stack(&[]);
            contested = stack.contested();
            live::paint_cloud_stack(&mut with, &stack);
            band_map = stack.flattened();
        }
        draw.push(started.elapsed());

        let s = measure(&bare, &with, band_map.as_ref(), pixels);
        if s.inked > 0 {
            cloud_frames += 1;
            lobe_max = lobe_max.max(s.lobes);
            crowd_max = crowd_max.max(contested);
            levels_seen = levels_seen.max(s.levels);
            ink_share_max = ink_share_max.max(100 * s.inked / s.footprint.max(1));
            worst_median = worst_median.max((s.after - s.before).abs());
            worst_mean = worst_mean.max((s.after_mean - s.before_mean).abs());
            worst_regional = worst_regional.max((s.after - s.elsewhere).abs());
            disturbed_total += s.disturbed;
            inked_total += s.inked;
            footprint_total += s.footprint;
            for (t, b) in band_totals.iter_mut().zip(s.by_band.iter()) {
                t.0 += b.0;
                t.1 += b.1;
            }
            // Only clouds big enough for "fog" to mean anything. A territory
            // whose whole banded region is two hundred pixels is a marker, and a
            // marker that is mostly ink is a marker, not a fill; the assertion
            // that matters is about a cloud large enough to hide a district.
            if s.footprint >= 400 {
                let through = 100 - 100 * s.inked / s.footprint;
                if through < show_through_min {
                    show_through_min = through;
                    worst_frame = (i, s.footprint, s.inked, s.by_band);
                }
            }
            // PRD §13's tween, measured: frame to frame the inked area has to
            // move by a fraction of itself, not by all of it. A step is what
            // "steppy" means.
            if let Some(p) = prev_cloud {
                if p > 0 {
                    jumps.push((s.inked as f64 - p as f64).abs() / p as f64);
                }
            }
            prev_cloud = Some(s.inked);
        } else {
            prev_cloud = None;
        }
        if i % 8 == 0 {
            eprintln!(
                "  frame {i:>3} @{target:>9} ms: {} threads -> {}, {} kernels, widest {:.1} px, 
                 {:>5} inked / {:>6} footprint = {:>2}%, {} levels, {} lobes,                  {contested} contested px, peak {peak:.2} | city shows 
                 through at L {:.2}, was L {:.2}, elsewhere L {:.2}; {} px disturbed | 
                 mean {:.2} -> {:.2}",
                snap.threads.len(),
                census.reason(),
                census.kernels,
                census.widest_px,
                s.inked,
                s.footprint,
                100 * s.inked / s.footprint.max(1),
                s.levels,
                s.lobes,
                s.after,
                s.before,
                s.elsewhere,
                s.disturbed,
                s.before_mean,
                s.after_mean,
            );
        }
        if let Some(dir) = &out {
            if i % 8 == 0 {
                with.write_png(&dir.join(format!("cloud-{i:03}.png")))
                    .expect("write");
                bare.write_png(&dir.join(format!("bare-{i:03}.png")))
                    .expect("write");
                shipped_frame
                    .write_png(&dir.join(format!("shipped-{i:03}.png")))
                    .expect("write");
            }
        }
    }
    draw.sort_unstable();
    jumps.sort_by(f64::total_cmp);
    let share = |(foot, ink): (usize, usize)| 100 * ink / foot.max(1);
    eprintln!(
        "WORST show-through: frame {}, footprint {} px, {} inked          (fringe {}/{}, body {}/{}, core {}/{})",
        worst_frame.0,
        worst_frame.1,
        worst_frame.2,
        worst_frame.3[0].1,
        worst_frame.3[0].0,
        worst_frame.3[1].1,
        worst_frame.3[1].0,
        worst_frame.3[2].1,
        worst_frame.3[2].0,
    );
    eprintln!(
        "BANDS fringe {:>7} px {:>2}% ink | body {:>7} px {:>2}% ink |          core {:>7} px {:>2}% ink",
        band_totals[0].0,
        share(band_totals[0]),
        band_totals[1].0,
        share(band_totals[1]),
        band_totals[2].0,
        share(band_totals[2]),
    );
    let per = |n: usize| n as f64 / census_frames.max(1) as f64;
    eprintln!(
        "SELECTION over {} frames, per frame: {:.2} shown, {:.2} not converged,          {:.2} dormant, {:.2} over cap; {:.1} kernels, widest {widest_max:.1} px.          {shown_frames} of {} frames had at least one cloud selected.",
        census_frames,
        per(census_total.shown),
        per(census_total.unplaced),
        per(census_total.dormant),
        per(census_total.capped),
        per(census_total.kernels),
        placements.len(),
    );
    eprintln!(
        "RESULT frames={} with_cloud={} shown_frames={} levels={} lobes_max={}          contested_max={} shipped_cloud_px={} ink_share_max={}% show_through_min={}%          disturbed_px={}          inked_overall={}% under_cloud_median_shift={:.3} under_cloud_mean_shift={:.3}          regional_median_gap={:.3} tween_p50={:.3} tween_p95={:.3}          draw_p50={:?} draw_p95={:?}",
        placements.len(),
        cloud_frames,
        shown_frames,
        levels_seen,
        lobe_max,
        crowd_max,
        shipped_cloud_px,
        ink_share_max,
        show_through_min,
        disturbed_total,
        100 * inked_total / footprint_total.max(1),
        worst_median,
        worst_mean,
        worst_regional,
        percentile(&jumps, 0.5),
        percentile(&jumps, 0.95),
        draw[draw.len() / 2],
        draw[draw.len() * 95 / 100],
    );
    // Every claim is **evaluated and printed before any of them panics**. The
    // first `assert!` in a list hides every claim after it, and a harness that
    // reports one failure of six is a harness that has to be run six times to
    // learn what it already knows. The report is the product; the panic is only
    // how a test says so.
    let overall = 100 * inked_total / footprint_total.max(1);
    let claims: [(bool, String); 6] = [
        (
            // The one this file exists for, and it is now a statement about the
            // **product**: the shipped selection, on real sessions, put a cloud
            // on the map. When it fails the SELECTION line above says why in
            // the policy's own words, and the fix is in `polis_world::territory`.
            shown_frames > 0,
            format!(
                "selection put a cloud on {shown_frames} of {} frames",
                placements.len()
            ),
        ),
        (
            cloud_frames > 0,
            format!("{cloud_frames} of those frames drew ink"),
        ),
        (
            // Not one pixel the city still shows through was changed. Marks are
            // opaque writes inside their own band, so a cloud cannot lift the
            // map by a single level, and this is the claim an alpha wash fails
            // on its very first pixel.
            disturbed_total == 0,
            format!("{disturbed_total} base-map px disturbed (want 0)"),
        ),
        (
            // Which is worth nothing unless there is a city left to show
            // through. A fill leaves none; PRD §10.4's bands have to leave most
            // of it.
            show_through_min >= 50,
            format!("{show_through_min}% of the ground under the worst cloud is still city (want >= 50)"),
        ),
        (
            (4..=40).contains(&overall),
            format!("the layer inks {overall}% of every banded pixel (want 4..=40)"),
        ),
        (
            // And the city that shows through reads at the luminance it had.
            worst_median <= 1.0,
            format!("the city under a cloud reads {worst_median:.3} levels differently (want <= 1.0)"),
        ),
    ];
    let mut failed = 0usize;
    for (ok, what) in &claims {
        eprintln!("  [{}] {what}", if *ok { "PASS" } else { "FAIL" });
        failed += usize::from(!*ok);
    }
    let _ = worst_mean;
    assert!(
        failed == 0,
        "{failed} of {} claims failed — see the [FAIL] lines, the BANDS line for          which iso band the ink is in, and the WORST line for the frame it is on",
        claims.len()
    );
}

/// Why the recorded GIFs have clouds the operator likes and the live window has
/// none — measured on one real session, at three cameras, on the same frame.
///
/// > *"the clouds looked so pretty in the gifs, now i dont see them"*
///
/// It is the same renderer in all three, and the same territory: the kernels,
/// the weights, the bandwidth, the cap and the selection are identical. What
/// differs is **how many output pixels one world unit is worth**, and every
/// visible property of this layer is a function of that one number:
///
/// * a Gaussian's radius in pixels is `bandwidth × scale`;
/// * the banded footprint is that radius **squared**;
/// * the contour width is fixed in *pixels*, so on a thin band it eats the
///   band — which is why a small cloud reads as a blob and a large one reads as
///   a contour map;
/// * and below `live`'s minimum stroke there is no cloud at all.
///
/// The three cameras:
///
/// | camera | what it is |
/// |---|---|
/// | `window` | the app's cloud raster: `polis_app::clouds::TEXELS` (1024) over the **whole** city, then magnified with a nearest filter |
/// | `gif` | `docs/replay`'s recorder: 1100 px over the robust bounding box of the paths the session touched |
/// | `headless` | 1100 px over the whole city — this harness's own default, for reference |
///
/// ```text
/// POLIS_REPO=qurio-toolset \
///   cargo test -p polis-render --release --test cloud_measure -- --ignored --nocapture gif_versus
/// ```
#[test]
#[ignore = "reads the operator's real sessions"]
fn gif_versus_window_is_a_difference_of_scale() {
    let Some(projects) = polis_world::sessions::default_projects_dir().filter(|p| p.is_dir())
    else {
        eprintln!("skipped: no ~/.claude/projects on this machine");
        return;
    };
    let index = SessionIndex::scan_with(&projects, &IndexOptions::quick()).expect("scan");
    let want = std::env::var("POLIS_REPO").unwrap_or_else(|_| "qurio-toolset".to_owned());
    let mut candidates: Vec<_> = index
        .sessions
        .iter()
        .filter(|s| s.repo_exists && s.repo.is_some())
        .filter(|s| {
            s.repo
                .as_ref()
                .is_some_and(|r| r.to_string_lossy().contains(&want))
        })
        .collect();
    candidates.sort_by_key(|s| std::cmp::Reverse(s.bytes));
    // `POLIS_SESSION` names the session by id prefix. It defaults to the one
    // `docs/replay/focus-qurio-services.gif` was recorded from, because the
    // whole question is why *that* file has clouds and the window does not, and
    // answering it on a different session would answer a different question.
    let session = std::env::var("POLIS_SESSION").unwrap_or_else(|_| "29c2fc6f".to_owned());
    let Some(sample) = candidates
        .iter()
        .find(|s| s.session.to_string().starts_with(&session))
        .or_else(|| candidates.first())
    else {
        eprintln!("skipped: no session under a repository matching {want}");
        return;
    };
    let repo_path = sample.repo.clone().expect("a repo");
    let repo = polis_repo::tree::RepoIndex::open(&repo_path).expect("index");
    let city = polis_layout::city::generate_city(repo.tree());
    let mapper = PathMapper::new(&repo_path).expect("mapper");
    let mut schedule = sample.schedule(&mapper).expect("offline full read");
    schedule.compress_idle_gaps(Some(polis_world::replay::DEFAULT_IDLE_GAP_CAP));
    eprintln!(
        "{} over {} — {} events, {:.1} min",
        sample.session,
        repo_path.display(),
        schedule.len(),
        schedule.duration_ms() as f64 / 60_000.0,
    );

    let gif_focus =
        recorder_focus(&city, &schedule).expect("too few placed paths to frame the work");
    eprintln!(
        "city extent {:.1}; gif camera centre ({:.1}, {:.1}) extent {:.1} \
         = {:.1}% of the city across",
        city.layout.extent,
        gif_focus.centre.x,
        gif_focus.centre.y,
        gif_focus.extent,
        100.0 * f64::from(gif_focus.extent) / f64::from(city.layout.extent),
    );

    // One world, three renderers. They see the same snapshot at the same
    // moment, so the territory — kernels, weights, bandwidth, selection — is
    // bit-identical in all three and the only variable left is the camera.
    let cameras: [(&str, usize, Option<Focus>); 3] = [
        ("window 1024/city", 1024, None),
        ("gif 1100/focus", 1100, Some(gif_focus)),
        ("headless 1100/city", 1100, None),
    ];
    let mut renderers: Vec<(&str, FrameRenderer)> = cameras
        .iter()
        .map(|(name, pixels, focus)| {
            (
                *name,
                FrameRenderer::new(
                    &city,
                    FrameOptions {
                        pixels: *pixels,
                        supersample: 1,
                        caption: false,
                        rail: false,
                        focus: *focus,
                        ..FrameOptions::default()
                    },
                ),
            )
        })
        .collect();

    let mut world = World::new(repo.tree().clone(), city.layout.clone());
    world.set_ubiquity(Box::new(DenylistUbiquity::default()));
    let mut driver = ReplayDriver::with_origin(schedule.clone(), Instant::now());
    let (publisher, reader) = snapshot::from_world(&world);
    // The moment the territory is heaviest. A cloud measured in the idle
    // between two bursts has already decayed under PRD §6.3's 90 s half-life,
    // and the question here is what the picture looks like at its *best* — the
    // operator is not complaining about a cloud that has finished dissipating.
    let plan = polis_render::pacing::plan(&schedule, 96);
    let mut at = plan.frames_ms[plan.frames_ms.len() / 2];
    {
        let mut scan = World::new(repo.tree().clone(), city.layout.clone());
        scan.set_ubiquity(Box::new(DenylistUbiquity::default()));
        let mut probe = ReplayDriver::with_origin(schedule.clone(), Instant::now());
        let mut best = 0usize;
        for target in plan.frames_ms.iter().copied() {
            probe.seek(target, &mut scan);
            let k: usize = scan
                .threads
                .values()
                .map(|t| t.territory.kernels.len())
                .max()
                .unwrap_or(0);
            if k > best {
                best = k;
                at = target;
            }
        }
        eprintln!(
            "heaviest frame: {best} kernels at {:.1} min",
            at as f64 / 60_000.0
        );
    }
    let out = std::env::var("POLIS_OUT")
        .ok()
        .map(std::path::PathBuf::from);
    if let Some(dir) = &out {
        std::fs::create_dir_all(dir).expect("output directory");
    }
    // The tween is a chase, so one frame at `dt` shows a sixth of the field.
    // Twenty-four of them is a settled second, which is what the operator is
    // looking at when they say there are no clouds.
    let dt = Duration::from_millis(1000 / 24);
    for step in 0..24 {
        driver.seek(at + step * 40, &mut world);
        publisher.force(&world);
        let snap = reader.load();
        for (_, r) in &mut renderers {
            r.render(&snap, dt);
        }
    }
    let snap = reader.load();
    eprintln!(
        "at {:.1} min: {} threads, heaviest territory has {} kernels",
        at as f64 / 60_000.0,
        snap.threads.len(),
        snap.threads
            .iter()
            .map(|t| t.territory.kernels.len())
            .max()
            .unwrap_or(0),
    );

    let mut sheet: Vec<(&str, Canvas)> = Vec::new();
    for (name, r) in &mut renderers {
        let px = r.map_pixels();
        let canvas = r.render_owned(&snap, Duration::ZERO);
        let census = r.cloud_census();
        let (footprint, thickness) = r.cloud_field().map_or((0, 0.0), |field| {
            let Some(bands) = field.stack(&[]).flattened() else {
                return (0, 0.0);
            };
            let mut foot = 0usize;
            let mut edge = [0usize; 3];
            let mut area = [0usize; 3];
            for y in 0..bands.height {
                for x in 0..bands.width {
                    let b = bands.cells[y * bands.width + x];
                    if b == NO_BAND {
                        continue;
                    }
                    foot += 1;
                    let k = b as usize;
                    area[k] += 1;
                    let differs = |nx: isize, ny: isize| -> bool {
                        if nx < 0
                            || ny < 0
                            || nx >= bands.width as isize
                            || ny >= bands.height as isize
                        {
                            return true;
                        }
                        bands.cells[ny as usize * bands.width + nx as usize] != b
                    };
                    let (ix, iy) = (x as isize, y as isize);
                    if differs(ix - 1, iy)
                        || differs(ix + 1, iy)
                        || differs(ix, iy - 1)
                        || differs(ix, iy + 1)
                    {
                        edge[k] += 1;
                    }
                }
            }
            // `2 · area / edge` is a band's own characteristic thickness — the
            // estimator `live::contour_steps` already sizes the contour from.
            // It is the number that decides whether a band has room for a
            // contour or *is* one.
            let thin = (0..3)
                .filter(|k| edge[*k] > 0)
                .map(|k| 2.0 * area[k] as f64 / edge[k] as f64)
                .fold(f64::MAX, f64::min);
            (
                foot,
                if thin.is_finite() && thin < f64::MAX {
                    thin
                } else {
                    0.0
                },
            )
        });
        let inked = canvas
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| CLOUD_TONES.contains(p))
            .count();
        eprintln!(
            "  {name:>19}: scale {:>7.3} px/unit, widest kernel {:>6.1} px, \
             footprint {:>7} px = {:>6.3}% of the map, {:>6} px of ink, \
             thinnest band {thickness:>5.1} px, ink share {:>3}% | {}",
            r.view().scale(),
            census.widest_px,
            footprint,
            100.0 * footprint as f64 / (px * px) as f64,
            inked,
            100 * inked / footprint.max(1),
            census.reason(),
        );
        if out.is_some() {
            sheet.push((*name, canvas));
        }
    }
    // One image, because the finding is a comparison. The panels keep their own
    // pixel sizes — 1024 and 1100 are what the two paths actually rasterise, and
    // scaling one to match the other would hide the very quantity being
    // compared.
    if let Some(dir) = &out {
        let gap = 16usize;
        let strip = 34usize;
        let w: usize = sheet.iter().map(|(_, c)| c.width).sum::<usize>() + gap * (sheet.len() - 1);
        let h = sheet.iter().map(|(_, c)| c.height).max().unwrap_or(0) + strip;
        let mut page = Canvas::new(w, h, [7, 8, 10]);
        let mut x0 = 0usize;
        for (name, c) in &sheet {
            for y in 0..c.height {
                let src = y * c.width * 3;
                let dst = (y * page.width + x0) * 3;
                page.pixels[dst..dst + c.width * 3]
                    .copy_from_slice(&c.pixels[src..src + c.width * 3]);
            }
            page.text(
                x0 as f64 + 8.0,
                (h - strip) as f64 + 10.0,
                &name.to_uppercase(),
                2.6,
                [150, 156, 164],
            );
            x0 += c.width + gap;
        }
        let path = dir.join("camera-scale.png");
        page.write_png(&path).expect("write");
        for (name, c) in &sheet {
            c.write_png(&dir.join(format!("scale-{}.png", name.replace([' ', '/'], "-"))))
                .expect("write");
        }
        eprintln!("wrote {}", path.display());
    }
}

/// The picture of the lift that used to be in this file, beside the picture the
/// product actually draws — same session, same city, same frame.
///
/// This exists because a header saying *"the harness re-implements the shipped
/// ranking and cap with that one gate lifted"* was true, was read, and still let
/// three GIFs and a page of measurements describe a cloud
/// `polis_render::frame` would not have drawn. A sentence is easy to skim past.
/// Two images side by side are not.
///
/// The left panel is the removed construction, verbatim: rank every thread by
/// `last_activity`, take six, splat **every** kernel, no convergence test, no
/// lobes test, no dormancy, no attention ranking. The right panel is
/// [`FrameRenderer`]'s own field — PRD §10.4's selection, cap and all.
///
/// It asserts nothing about which is prettier. It reports the difference in
/// kernels and ink, which is the number that says how much of the old
/// measurement was measuring the product.
///
/// ```text
/// POLIS_OUT=<dir> POLIS_REPO=qurio-toolset POLIS_THREADS=3 \
///   cargo test -p polis-render --release --test cloud_measure -- --ignored --nocapture lift_versus
/// ```
#[test]
#[ignore = "reads the operator's real sessions and writes an image"]
fn lift_versus_shipped_side_by_side() {
    let Some(dir) = std::env::var("POLIS_OUT")
        .ok()
        .map(std::path::PathBuf::from)
    else {
        eprintln!("skipped: set POLIS_OUT to a directory");
        return;
    };
    std::fs::create_dir_all(&dir).expect("output directory");
    let Some((city, repo, schedule, label)) = overlaid_world() else {
        return;
    };
    let pixels: usize = std::env::var("POLIS_PIXELS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(900);

    let mut world = World::new(repo.tree().clone(), city.layout.clone());
    world.set_ubiquity(Box::new(DenylistUbiquity::default()));
    let mut driver = ReplayDriver::with_origin(schedule.clone(), Instant::now());
    // The recorder's camera, because the notation is only legible at PRD §12's
    // district tier and this image exists to be looked at. At the city tier the
    // difference between the two panels is a few hundred pixels of ink in a
    // corner, which is itself the finding of `gif_versus_window` and not what
    // this test is for.
    let opts = FrameOptions {
        pixels,
        supersample: 1,
        caption: false,
        rail: false,
        focus: recorder_focus(&city, &schedule),
        ..FrameOptions::default()
    };
    let mut bare_r = FrameRenderer::new(
        &city,
        FrameOptions {
            cloud_cap: 0,
            ..opts
        },
    );
    let mut shipped_r = FrameRenderer::new(&city, opts);
    let (publisher, reader) = snapshot::from_world(&world);
    let plan = polis_render::pacing::plan(&schedule, 64);

    let dt = Duration::from_millis(1000 / 24);
    let mut chosen: Option<(u64, Canvas, Canvas, usize, usize, usize)> = None;
    let mut widest_gap: Option<usize> = None;
    let mut tween = CloudTween::default();
    for target in plan.frames_ms.iter().copied() {
        driver.seek(target, &mut world);
        publisher.force(&world);
        let snap = reader.load();
        let bare = bare_r.render_owned(&snap, dt);
        let shipped = shipped_r.render_owned(&snap, dt);
        let census = shipped_r.cloud_census();

        // The removed lift, verbatim, and the only copy of it left anywhere.
        let mut ranked: Vec<&polis_world::Thread> = snap.threads.iter().collect();
        ranked.sort_by(|a, b| b.last_activity.cmp(&a.last_activity).then(a.id.cmp(&b.id)));
        let view = *bare_r.view();
        let mut kernels: Vec<CloudKernel> = Vec::new();
        for (rank, thread) in ranked.iter().take(6).enumerate() {
            for k in &thread.territory.kernels {
                if k.weight <= 0.0 {
                    continue;
                }
                kernels.push(CloudKernel {
                    at: view.at(k.centre),
                    radius: f64::from(k.radius) * view.scale(),
                    weight: f64::from(k.weight),
                    thread: rank as u16,
                });
            }
        }
        let ink = |c: &Canvas| {
            c.pixels
                .as_chunks::<3>()
                .0
                .iter()
                .filter(|p| CLOUD_TONES.contains(p))
                .count()
        };
        let lifted_n = kernels.len();
        let mut lifted = bare.clone();
        if let Some(field) = tween.advance(
            CloudField::sample(&kernels, pixels, pixels),
            1.0 / 24.0,
            live::CLOUD_TWEEN_RATE,
        ) {
            live::paint_cloud_stack(&mut lifted, &field.stack(&[]));
        }
        // The right panel is the shipped **field**, painted onto the same bare
        // frame at the same point in the stack as the left. Comparing the
        // composed frame instead would compare two different things: PRD
        // §10.3 puts the clouds *under* the labels and the agent layer, so in
        // the finished image the tether fan and the marks sit on top of the
        // cloud and hide a good share of it. That is worth knowing — it is
        // reported below — but it is not the difference this image is about.
        let mut shipped_panel = bare.clone();
        if let Some(field) = shipped_r.cloud_field() {
            live::paint_cloud_stack(&mut shipped_panel, &field.stack(&[]));
        }
        let covered = ink(&shipped_panel).saturating_sub(ink(&shipped));

        // The frame where the two panels differ most *in ink*, not in kernels:
        // the picture is only worth writing at the moment it has something to
        // show, and a kernel that lands under another kernel shows nothing.
        let gap = ink(&lifted).abs_diff(ink(&shipped_panel));
        if widest_gap.is_none_or(|w| gap >= w) {
            widest_gap = Some(gap);
            chosen = Some((
                target,
                lifted,
                shipped_panel,
                lifted_n,
                census.kernels,
                covered,
            ));
        }
    }
    let Some((at, lifted, shipped, lifted_n, shipped_n, covered)) = chosen else {
        eprintln!("no frame to compare");
        return;
    };
    let ink = |c: &Canvas| {
        c.pixels
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| CLOUD_TONES.contains(p))
            .count()
    };
    let (li, si) = (ink(&lifted), ink(&shipped));
    eprintln!(
        "{label}\n  at {:.1} min: LIFTED {lifted_n} kernels, {li} px of cloud ink | \
         SHIPPED {shipped_n} kernels, {si} px of cloud ink | in the composed frame \
         layer 4 covers {covered} px = {}% of the shipped cloud",
        at as f64 / 60_000.0,
        100 * covered / si.max(1),
    );

    // One image, two panels, a rule between them and a caption under each.
    let gap = 16usize;
    let strip = 34usize;
    let mut sheet = Canvas::new(pixels * 2 + gap, pixels + strip, [7, 8, 10]);
    for (panel, src) in [&lifted, &shipped].into_iter().enumerate() {
        let x0 = panel * (pixels + gap);
        for y in 0..pixels {
            let s = y * src.width * 3;
            let d = (y * sheet.width + x0) * 3;
            sheet.pixels[d..d + pixels * 3].copy_from_slice(&src.pixels[s..s + pixels * 3]);
        }
    }
    let size = 2.6;
    sheet.text(
        8.0,
        pixels as f64 + 10.0,
        &format!("LIFTED GATE  {lifted_n} KERNELS  {li} PX INK"),
        size,
        [150, 156, 164],
    );
    sheet.text(
        (pixels + gap) as f64 + 8.0,
        pixels as f64 + 10.0,
        &format!("SHIPPED PATH  {shipped_n} KERNELS  {si} PX INK"),
        size,
        [150, 156, 164],
    );
    let path = dir.join("lift-versus-shipped.png");
    sheet.write_png(&path).expect("write");
    eprintln!("wrote {}", path.display());
}
