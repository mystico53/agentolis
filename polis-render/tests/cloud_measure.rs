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
//! | `POLIS_OUT` | directory to write the measured frames into, if wanted |
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
//! # What this harness cannot show, and whose it is
//!
//! No real session in `~/.claude/projects` produces a **converged** territory:
//! PRD §6.2 needs the weight-trimmed lowest common ancestor at `depth >= 2`, and
//! a session that works across a repository has the repository as its ancestor,
//! `depth 0`. `polis_render::frame` honours that gate, so the shipped path draws
//! no cloud on any of these frames. The harness therefore drives the layer from
//! the same real kernels with that one gate lifted, and prints how many threads
//! passed it so the difference is never invisible. The gate lives in
//! `polis_world::territory` and is not this crate's to move.

// A measurement harness: counts, ratios, and one long linear script whose order
// is part of what it is measuring.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use polis_events::PathMapper;
use polis_render::frame::{FrameOptions, FrameRenderer};
use polis_render::live::{
    self, BandMap, CloudField, CloudKernel, CloudTween, CLOUD_CROWD, CLOUD_TONES, NO_BAND,
};
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
    let mut before: Vec<f64> = Vec::new();
    let mut after: Vec<f64> = Vec::new();
    let mut elsewhere: Vec<f64> = Vec::new();
    for y in 0..rows {
        for x in 0..w {
            let under = covered[y * w + x];
            if under {
                footprint += 1;
                if cloud[y * w + x] {
                    inked += 1;
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
    }
}

/// Overlays several real sessions of one repository into one world and measures
/// the cloud layer frame by frame.
#[test]
#[ignore = "reads the operator's real sessions"]
fn measure_clouds_on_real_threads() {
    let Some(projects) = polis_world::sessions::default_projects_dir().filter(|p| p.is_dir())
    else {
        eprintln!("skipped: no ~/.claude/projects on this machine");
        return;
    };
    let index = SessionIndex::scan_with(&projects, &IndexOptions::quick()).expect("scan");
    let want = std::env::var("POLIS_REPO").unwrap_or_else(|_| "qurio-toolset".to_owned());
    let threads: usize = std::env::var("POLIS_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let frames: usize = std::env::var("POLIS_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64);
    let pixels: usize = std::env::var("POLIS_PIXELS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(900);

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
        return;
    }
    let repo_path = candidates[0].repo.clone().expect("a repo");
    let repo = polis_repo::tree::RepoIndex::open(&repo_path).expect("index");
    let city = polis_layout::city::generate_city(repo.tree());
    let mapper = PathMapper::new(&repo_path).expect("mapper");

    // Overlay: each session is shifted to start at zero, so sessions that ran
    // days apart become threads running side by side. Their *paths* are real and
    // so is everything the territory inference does with them; only the start
    // times move, and they move to produce the arrangement PRD §6.4 is about —
    // several territories on one map at one moment.
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
    eprintln!(
        "{} threads over {} — {} events, {:.1} min overlaid",
        spans.len(),
        repo_path.display(),
        schedule.len(),
        schedule.duration_ms() as f64 / 60_000.0,
    );
    for (id, last, n) in &spans {
        eprintln!("  {id}: {n} events, {:.1} min", *last as f64 / 60_000.0);
    }

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

    let mut tween = CloudTween::default();
    let out = std::env::var("POLIS_OUT")
        .ok()
        .map(std::path::PathBuf::from);
    if let Some(dir) = &out {
        std::fs::create_dir_all(dir).expect("output directory");
    }

    let mut cloud_frames = 0usize;
    let mut converged_frames = 0usize;
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
    let mut prev_cloud: Option<usize> = None;
    let mut jumps: Vec<f64> = Vec::new();
    let mut draw: Vec<Duration> = Vec::new();
    for (i, target) in placements.iter().copied().enumerate() {
        driver.seek(target, &mut world);
        publisher.force(&world);
        let snap = reader.load();
        let bare = renderer.render_owned(&snap, Duration::from_millis(1000 / 24));
        let shipped_frame = shipped.render_owned(&snap, Duration::from_millis(1000 / 24));
        let shipped_px = shipped_frame
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| CLOUD_TONES.contains(p))
            .count();
        shipped_cloud_px = shipped_cloud_px.max(shipped_px);
        let converged = snap
            .threads
            .iter()
            .filter(|t| t.territory.claim.is_some())
            .count();
        converged_frames += usize::from(converged > 0);

        // The shipped ranking and cap, with PRD §6.2's convergence gate lifted.
        let mut ranked: Vec<&polis_world::Thread> = snap.threads.iter().collect();
        ranked.sort_by(|a, b| b.last_activity.cmp(&a.last_activity).then(a.id.cmp(&b.id)));
        let view = *renderer.view();
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
        let target_field = CloudField::sample(&kernels, pixels, pixels);
        let radii = (
            kernels.iter().map(|k| k.radius).fold(0.0f64, f64::max),
            kernels.iter().map(|k| k.radius).fold(f64::MAX, f64::min),
        );
        let radii = if kernels.is_empty() {
            (0.0, 0.0)
        } else {
            (radii.1, radii.0)
        };
        let peak = target_field.as_ref().map_or(0.0, CloudField::peak);
        let mut with = bare.clone();
        let started = Instant::now();
        let mut contested = 0usize;
        let mut band_map = None;
        if let Some(field) = tween.advance(target_field, 1.0 / 24.0, live::CLOUD_TWEEN_RATE) {
            let bands = field.bands();
            contested = bands
                .crowd
                .iter()
                .zip(bands.cells.iter())
                .filter(|(c, b)| **c >= CLOUD_CROWD && **b != NO_BAND)
                .count();
            live::paint_cloud_bands(&mut with, &bands);
            band_map = Some(bands);
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
            // Only clouds big enough for "fog" to mean anything. A territory
            // whose whole banded region is two hundred pixels is a marker, and a
            // marker that is mostly ink is a marker, not a fill; the assertion
            // that matters is about a cloud large enough to hide a district.
            if s.footprint >= 400 {
                show_through_min = show_through_min.min(100 - 100 * s.inked / s.footprint);
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
                "  frame {i:>3} @{target:>9} ms: {} threads ({converged} converged), \
                 {} kernels, {:>5} inked / {:>6} footprint = {:>2}%, {} levels, {} lobes, \
                 {contested} contested px | r {:.1}-{:.1} px, peak {peak:.2} | city shows \n                 through at L {:.2}, was L {:.2}, elsewhere L {:.2}; {} px disturbed | \n                 mean {:.2} -> {:.2}",
                snap.threads.len(),
                kernels.len(),
                s.inked,
                s.footprint,
                100 * s.inked / s.footprint.max(1),
                s.levels,
                s.lobes,
                radii.0,
                radii.1,
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
    eprintln!(
        "RESULT frames={} with_cloud={} converged_frames={} levels={} lobes_max={} \
         contested_max={} shipped_cloud_px={} ink_share_max={}% show_through_min={}% \
         disturbed_px={} \
         inked_overall={}% under_cloud_median_shift={:.3} under_cloud_mean_shift={:.3} \
         regional_median_gap={:.3} tween_p50={:.3} tween_p95={:.3} \
         draw_p50={:?} draw_p95={:?}",
        placements.len(),
        cloud_frames,
        converged_frames,
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
    assert!(cloud_frames > 0, "no cloud was drawn in any frame");
    // The claim, in three parts.
    //
    // 1. Not one pixel the city still shows through was changed. Marks are
    //    opaque writes inside their own band, so a cloud cannot lift the map
    //    by a single level, and this is the assertion an alpha wash fails on
    //    its very first pixel.
    assert_eq!(
        disturbed_total, 0,
        "the cloud changed base-map pixels it did not fully own"
    );
    // 2. Which is worth nothing unless there is a city left to show through.
    //    A fill leaves none; PRD §10.4's bands have to leave most of it.
    assert!(
        show_through_min >= 50,
        "only {show_through_min}% of the ground under a cloud is still the city: a fill"
    );
    let overall = 100 * inked_total / footprint_total.max(1);
    assert!(
        overall <= 40,
        "the cloud layer inks {overall}% of every banded pixel it drew: that is a fill"
    );
    assert!(overall >= 4, "the cloud layer inks {overall}%: invisible");
    // 3. And the city that shows through reads at the luminance it had.
    assert!(
        worst_median <= 1.0,
        "the city under a cloud reads {worst_median:.2} levels differently: \
         the cloud is raising the floor"
    );
    let _ = worst_mean;
}
