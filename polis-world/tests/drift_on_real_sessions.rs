//! PRD §10.4's drift signal, measured against the operator's own sessions.
//!
//! > **Drift is the payoff.** A territory whose centre of mass is migrating out
//! > of `src/auth` toward `tests/` has a scope that is changing, and it is
//! > visible *while it is happening* — well before contention fires. […] This is
//! > the redirect signal, and it is the one thing here that no existing tool
//! > provides.
//!
//! A signal that fires constantly is noise; one that never fires is decoration.
//! PRD §17 asks the same question of every visual element — *"does it change a
//! decision?"* — so the threshold has to come from measurement, and the
//! measurement has to name sessions.
//!
//! # The ground truth, and why it is not circular
//!
//! Drift is a **geometric** statement: the centre of mass of the density field
//! has moved. The territory *claim* is a **path** statement, arrived at by a
//! completely separate route — PRD §6.2's weight-trimmed lowest common ancestor
//! over the evidence, gated on depth and mass, and held by §6.3's hysteresis
//! until eight consecutive observations fall outside it. When the claim jumps
//! from one district to a district that is neither its ancestor nor its
//! descendant, the agent's scope demonstrably moved, and it moved on evidence
//! drift never sees. That is the label this measures against.
//!
//! ```text
//! cargo test -p polis-world --release --test drift_on_real_sessions -- --ignored --nocapture
//! ```
//!
//! | env | meaning |
//! |---|---|
//! | `POLIS_SESSIONS` | how many sessions to survey (default 16) |
//! | `POLIS_REPO` | substring of the repository path to restrict to |

// A measurement harness: the numbers are counts and ratios, and the survey is
// deliberately one linear script so the order of the measurements is visible.
#![allow(
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    clippy::similar_names
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use polis_events::PathMapper;
use polis_layout::CityLayout;
use polis_world::replay::ReplayDriver;
use polis_world::sessions::{IndexOptions, SessionIndex, SessionSummary};
use polis_world::territory::{Drift, DRIFT_COHERENCE, DRIFT_MIN_RECENT, DRIFT_THRESHOLD};
use polis_world::{DenylistUbiquity, World};

/// How often the world is sampled, in session time.
const SAMPLE: Duration = Duration::from_secs(5);

/// The most samples taken from one session, whatever its nominal length.
const MAX_SAMPLES: u128 = 6_000;

/// How close to a claim move a firing episode has to be to count as having
/// predicted it.
///
/// Generous on purpose. PRD §10.4's claim is that drift is visible *"well before
/// contention fires"*, and PRD §6.3 makes the claim itself wait for eight
/// consecutive outside observations, so the geometry is expected to lead the
/// path statement by a lot. A window that only counted the last few seconds
/// would be measuring the hysteresis, not the signal.
const LEAD_WINDOW: f64 = 300.0;

/// The share of live evidence weight the leading district must hold before it
/// counts as *the* scope. Below this the argmax is a coin flip between
/// near-ties, and every label built on it is noise.
const MODE_SHARE: f32 = 0.5;

/// How deep a path is truncated before two of them count as different places.
///
/// Two components: `src/components` rather than `src/components/GmailWindow`.
/// That is the granularity a district on the map has, and PRD §6.2's
/// `MIN_CLAIM_DEPTH` is the same number for the same reason.
const DISTRICT_DEPTH: usize = 2;

/// The first [`DISTRICT_DEPTH`] components of a path.
fn district_of(path: &str) -> &str {
    match path.match_indices('/').nth(DISTRICT_DEPTH - 1) {
        Some((i, _)) => &path[..i],
        None => path,
    }
}

/// How far ahead the lift test looks for the scope actually having moved.
const HORIZON: f64 = 300.0;

/// Thresholds swept, as multiples of the bandwidth.
const SWEEP: [f32; 5] = [0.25, 0.5, 1.0, 2.0, 4.0];

/// One sample of one thread's territory.
#[derive(Debug, Clone)]
struct Sample {
    /// Session seconds since the first event.
    at: f64,
    claim: Option<String>,
    drift: Option<Drift>,
    /// The directory holding the most live evidence weight, with **no**
    /// hysteresis and no depth gate. The claim is what PRD §6.2 and §6.3 will
    /// admit to; this is what the agent is actually working on right now, and it
    /// is the dense label the lift test needs.
    mode: Option<String>,
}

/// Signal-detection numbers at one threshold.
#[derive(Debug, Clone, Default)]
struct Detection {
    /// Samples firing, over samples with a claim. The noise floor: a mark up
    /// this fraction of the time is what the operator actually sees.
    duty: f64,
    /// Maximal runs of consecutive firing samples.
    episodes: usize,
    /// Episodes ending within [`LEAD_WINDOW`] of a lateral claim move.
    episodes_hit: usize,
    /// Lateral claim moves preceded by a firing episode.
    moves_caught: usize,
    /// Median lead, in session seconds, of a caught move.
    median_lead: f64,
    /// Each move's time, its distance back to the nearest preceding episode
    /// (unwindowed), and what moved.
    nearest: Vec<(f64, Option<f64>, String)>,
    /// Every firing episode, as `(start, end)` in session seconds.
    episode_spans: Vec<(f64, f64)>,
}

/// What one session looked like over its whole run.
#[derive(Debug)]
struct SessionReport {
    id: String,
    label: String,
    samples: usize,
    step_s: f64,
    /// Samples where a claim existed at all — the only ones a cloud is drawn
    /// for, and therefore the only ones drift can be judged on.
    claimed: usize,
    /// Distinct claims, in the order they were first held.
    claim_track: Vec<String>,
    /// Claim changes to a district that is neither ancestor nor descendant of
    /// the previous one. The ground truth for "the scope genuinely moved".
    moves: Vec<(f64, String)>,
    /// Ratio percentiles over claimed samples.
    p50: f32,
    p90: f32,
    max_ratio: f32,
    /// Detection numbers per swept threshold, coherence gate applied.
    detection: BTreeMap<String, Detection>,
    /// Every sample that had a claim, kept for the lift test.
    claimed_samples: Vec<Sample>,
}

fn percentile(sorted: &[f32], q: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let last = sorted.len() - 1;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // `q` is a quantile in [0, 1] and `last` is a sample index, so the product
    // is a non-negative index inside the slice.
    let idx = ((last as f32) * q).round() as usize;
    sorted[idx.min(last)]
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// Whether `b` is a genuinely different district from `a` rather than the same
/// one widening or narrowing.
fn lateral(a: &str, b: &str) -> bool {
    !(a.starts_with(b) || b.starts_with(a))
}

/// Runs of consecutive firing samples, as `(start, end)` in session seconds.
fn episodes(
    samples: &[&Sample],
    step: f64,
    fires: impl Fn(Drift) -> bool + Copy,
) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::new();
    // Twice the step: one missed sample inside a run is still one episode, and
    // a genuine gap is much larger than that.
    let tolerance = step * 2.5;
    for s in samples {
        if !s.drift.is_some_and(fires) {
            continue;
        }
        match out.last_mut() {
            Some(last) if s.at - last.1 <= tolerance => last.1 = s.at,
            _ => out.push((s.at, s.at)),
        }
    }
    out
}

fn detect(samples: &[&Sample], moves: &[(f64, String)], step: f64, threshold: f32) -> Detection {
    let fires = |d: Drift| {
        d.ratio > threshold && d.coherence >= DRIFT_COHERENCE && d.recent_n >= DRIFT_MIN_RECENT
    };
    let firing = samples
        .iter()
        .filter(|s| s.drift.is_some_and(fires))
        .count();
    let eps = episodes(samples, step, fires);
    let mut episodes_hit = 0;
    for (_, end) in &eps {
        if moves
            .iter()
            .any(|(when, _)| *when >= *end && *when - *end <= LEAD_WINDOW)
        {
            episodes_hit += 1;
        }
    }
    let mut leads = Vec::new();
    for (when, _) in moves {
        // The episode that ends closest before the move, inside the window.
        let best = eps
            .iter()
            .filter(|(start, end)| *end <= *when && *when - *start <= LEAD_WINDOW + step)
            .map(|(start, _)| *when - *start)
            .fold(None::<f64>, |acc, l| Some(acc.map_or(l, |a| a.max(l))));
        if let Some(lead) = best {
            leads.push(lead);
        }
    }
    // Every move's distance to the nearest preceding episode, with **no**
    // window at all. `LEAD_WINDOW` is an assumption and this is the measurement
    // that checks it: if the leads are all enormous, the window is wrong; if
    // they are all `None`, the signal genuinely does not precede the move.
    let mut nearest = Vec::new();
    for (when, label) in moves {
        let best = eps
            .iter()
            .filter(|(_, end)| *end <= *when)
            .map(|(_, end)| *when - *end)
            .fold(None::<f64>, |acc, l| Some(acc.map_or(l, |a: f64| a.min(l))));
        nearest.push((*when, best, label.clone()));
    }

    Detection {
        duty: firing as f64 / samples.len().max(1) as f64,
        episodes: eps.len(),
        episodes_hit,
        moves_caught: leads.len(),
        median_lead: median(leads.clone()),
        nearest,
        episode_spans: eps,
    }
}

fn survey(sample: &SessionSummary, city: &CityLayout, repo: &Path) -> Option<SessionReport> {
    let mapper = PathMapper::new(repo).ok()?;
    // No idle-gap compression: with the schedule left at its true pacing,
    // schedule time *is* session time, so a forward seek every `SAMPLE` steps
    // the world exactly the way the operator's clock would.
    let mut schedule = sample.schedule(&mapper).ok()?;
    schedule.compress_idle_gaps(None);
    if schedule.is_empty() {
        return None;
    }
    let duration_ms = schedule.duration_ms();
    let origin = Instant::now();
    let mut world = World::for_replay(city.clone());
    world.set_ubiquity(Box::new(DenylistUbiquity::default()));
    let mut driver = ReplayDriver::with_origin(schedule, origin);

    let mut samples: Vec<Sample> = Vec::new();
    // A transcript's timestamps step backwards on 20% of files (ADR-0014) and
    // the running maximum that repairs the order can leave a session whose
    // nominal duration is months. Sampling that at 5 s would be a billion seeks,
    // so the step widens rather than the survey hanging.
    let step_ms = u64::try_from(
        SAMPLE
            .as_millis()
            .max(u128::from(duration_ms) / MAX_SAMPLES),
    )
    .unwrap_or(u64::MAX);
    let mut at_ms = 0u64;
    loop {
        // One `tick` per sample rather than one per event: the world's decay is
        // a function of time, not of how chatty the session was, so this is the
        // same world the renderer would see at that moment.
        driver.seek(at_ms, &mut world);
        // The busiest thread is the one whose cloud the operator is looking at.
        if let Some(thread) = world
            .threads
            .values()
            .max_by(|a, b| a.territory.mass().total_cmp(&b.territory.mass()))
        {
            // Districts, not leaf directories: `src/components/GmailWindow` and
            // `src/components/DriveWindow` are the same place on the map, and a
            // label that calls a move between them a scope change is measuring
            // argmax flicker. And only when the leader is decisive - a mode
            // holding a third of the weight against a near-tie is noise.
            let mut by_dir: BTreeMap<&str, f32> = BTreeMap::new();
            let mut total = 0.0f32;
            for e in &thread.territory.evidence {
                *by_dir.entry(district_of(e.path.as_str())).or_default() += e.weight;
                total += e.weight;
            }
            let mode = by_dir
                .into_iter()
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .filter(|(_, w)| total > 0.0 && *w / total >= MODE_SHARE)
                .map(|(p, _)| p.to_owned());
            samples.push(Sample {
                at: at_ms as f64 / 1000.0,
                claim: thread
                    .territory
                    .claim
                    .as_ref()
                    .map(|c| c.as_str().to_owned()),
                drift: thread.territory.drift_state(),
                mode,
            });
        }
        if at_ms >= duration_ms {
            break;
        }
        at_ms = (at_ms + step_ms).min(duration_ms);
    }
    if samples.is_empty() {
        return None;
    }

    let mut claim_track: Vec<String> = Vec::new();
    let mut moves: Vec<(f64, String)> = Vec::new();
    for s in &samples {
        let Some(claim) = &s.claim else { continue };
        match claim_track.last() {
            Some(prev) if prev == claim => {}
            Some(prev) => {
                if lateral(prev, claim) {
                    moves.push((s.at, format!("{prev} -> {claim}")));
                }
                claim_track.push(claim.clone());
            }
            None => claim_track.push(claim.clone()),
        }
    }

    let claimed: Vec<&Sample> = samples.iter().filter(|s| s.claim.is_some()).collect();
    let mut ratios: Vec<f32> = claimed
        .iter()
        .map(|s| s.drift.map_or(0.0, |d| d.ratio))
        .collect();
    ratios.sort_by(f32::total_cmp);

    let step_s = step_ms as f64 / 1000.0;
    let mut detection = BTreeMap::new();
    for t in SWEEP {
        detection.insert(format!("{t:.1}"), detect(&claimed, &moves, step_s, t));
    }

    Some(SessionReport {
        id: sample.session.as_str().chars().take(8).collect(),
        label: sample.label(),
        samples: samples.len(),
        step_s,
        claimed: claimed.len(),
        claim_track,
        moves,
        p50: percentile(&ratios, 0.50),
        p90: percentile(&ratios, 0.90),
        max_ratio: ratios.last().copied().unwrap_or(0.0),
        detection,
        claimed_samples: claimed.into_iter().cloned().collect(),
    })
}

#[test]
#[ignore = "reads the operator's real sessions"]
fn survey_drift_across_the_corpus() {
    let Some(projects) = polis_world::sessions::default_projects_dir().filter(|p| p.is_dir())
    else {
        eprintln!("skipped: no ~/.claude/projects on this machine");
        return;
    };
    let index = SessionIndex::scan_with(&projects, &IndexOptions::quick()).expect("scan");
    let want_repo = std::env::var("POLIS_REPO").unwrap_or_default();
    let limit: usize = std::env::var("POLIS_SESSIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);

    let mut candidates: Vec<&SessionSummary> = index
        .sessions
        .iter()
        .filter(|s| s.repo_exists && s.repo.is_some() && s.is_replayable())
        .filter(|s| {
            want_repo.is_empty()
                || s.repo
                    .as_ref()
                    .is_some_and(|r| r.to_string_lossy().contains(&want_repo))
        })
        .collect();
    candidates.sort_by_key(|s| std::cmp::Reverse(s.bytes));
    candidates.truncate(limit);
    eprintln!("surveying {} sessions", candidates.len());

    let mut cities: BTreeMap<PathBuf, CityLayout> = BTreeMap::new();
    let mut reports = Vec::new();
    for sample in candidates {
        let Some(repo_path) = sample.repo.clone() else {
            continue;
        };
        if !cities.contains_key(&repo_path) {
            let Ok(repo) = polis_repo::tree::RepoIndex::open(&repo_path) else {
                continue;
            };
            let built = Instant::now();
            let city = polis_layout::city::generate(repo.tree());
            eprintln!(
                "  city for {}: {} buildings in {:?}",
                repo_path.display(),
                city.buildings.len(),
                built.elapsed()
            );
            cities.insert(repo_path.clone(), city);
        }
        let city = &cities[&repo_path];
        if let Some(report) = survey(sample, city, &repo_path) {
            reports.push(report);
        }
    }

    eprintln!("\n{:-<120}", "");
    eprintln!(
        "{:<10} {:>6} {:>6} {:>7} {:>7} {:>7} {:>6} {:>6} {:>6} {:>6}  label",
        "session", "smpl", "step", "claimed", "p50", "p90", "max", "duty%", "moves", "caught",
    );
    let key = format!("{DRIFT_THRESHOLD:.1}");
    for r in &reports {
        let d = &r.detection[&key];
        eprintln!(
            "{:<10} {:>6} {:>5.0}s {:>7} {:>7.2} {:>7.2} {:>6.1} {:>5.1}% {:>6} {:>6}  {}",
            r.id,
            r.samples,
            r.step_s,
            r.claimed,
            r.p50,
            r.p90,
            r.max_ratio,
            d.duty * 100.0,
            r.moves.len(),
            d.moves_caught,
            r.label.chars().take(42).collect::<String>(),
        );
    }

    eprintln!("\nclaim tracks (the ground truth) and what drift did at {key}x bandwidth");
    for r in &reports {
        let d = &r.detection[&key];
        eprintln!(
            "  {} [{}]\n    track: {}",
            r.id,
            r.label.chars().take(56).collect::<String>(),
            if r.claim_track.is_empty() {
                "(never converged)".to_owned()
            } else {
                r.claim_track.join(" -> ")
            }
        );
        eprintln!(
            "    {} episode(s), {} of them within {LEAD_WINDOW:.0}s of a scope move; \
             {}/{} moves caught, median lead {:.0}s",
            d.episodes,
            d.episodes_hit,
            d.moves_caught,
            r.moves.len(),
            d.median_lead,
        );
        for (when, gap, label) in &d.nearest {
            eprintln!(
                "      move at {when:.0}s ({label}) — nearest earlier episode ended {} before",
                gap.map_or_else(|| "never".to_owned(), |g| format!("{g:.0}s")),
            );
        }
        let spans: Vec<String> = d
            .episode_spans
            .iter()
            .take(8)
            .map(|(a, b)| format!("{a:.0}-{b:.0}"))
            .collect();
        if !spans.is_empty() {
            eprintln!(
                "      episodes at {}s{}",
                spans.join(", "),
                if d.episode_spans.len() > 8 {
                    " ..."
                } else {
                    ""
                }
            );
        }
    }

    eprintln!("\nthreshold sweep over the whole corpus (straightness gate applied)");
    eprintln!(
        "{:>7} {:>8} {:>10} {:>12} {:>10} {:>12}",
        "ratio", "duty%", "episodes", "ep. on-move", "moves", "moves caught"
    );
    let total_moves: usize = reports.iter().map(|r| r.moves.len()).sum();
    let claimed: usize = reports.iter().map(|r| r.claimed).sum();
    for t in SWEEP {
        let k = format!("{t:.1}");
        let firing: f64 = reports
            .iter()
            .map(|r| r.detection[&k].duty * r.claimed as f64)
            .sum();
        let eps: usize = reports.iter().map(|r| r.detection[&k].episodes).sum();
        let hit: usize = reports.iter().map(|r| r.detection[&k].episodes_hit).sum();
        let caught: usize = reports.iter().map(|r| r.detection[&k].moves_caught).sum();
        eprintln!(
            "{t:>7.1} {:>7.1}% {eps:>10} {:>11.0}% {total_moves:>10} {:>11.0}%",
            100.0 * firing / claimed.max(1) as f64,
            100.0 * hit as f64 / eps.max(1) as f64,
            100.0 * caught as f64 / total_moves.max(1) as f64,
        );
    }

    // ---------------------------------------------------------------------
    // The lift test, and it is the one that decides whether the mark stays.
    //
    // The claim is a poor label: PRD §6.3 holds it against eight consecutive
    // outside observations and PRD §6.2 will not emit one below depth 2, so a
    // session's claim track is a sparse, delayed shadow of what it was doing.
    // `Sample::mode` is the same statement with neither gate applied - the
    // directory holding the most live evidence weight, right now.
    //
    // A sample is **followed by a move** when the mode `HORIZON` seconds later
    // is neither an ancestor nor a descendant of the mode now. The base rate is
    // that over every claimed sample; the precision is that over the samples
    // where drift is firing. Lift is precision over base rate, and a lift of one
    // means the mark carries no information about where the agent is going.
    // ---------------------------------------------------------------------
    eprintln!();
    eprintln!("lift against the unhysteresed scope mode, {HORIZON:.0}s ahead");
    eprintln!(
        "{:>7} {:>10} {:>11} {:>10} {:>7}",
        "ratio", "base rate", "precision", "firing n", "lift"
    );
    for t in SWEEP {
        let (mut base_hit, mut base_n, mut fire_hit, mut fire_n) = (0usize, 0usize, 0usize, 0usize);
        for r in &reports {
            for (i, s) in r.claimed_samples.iter().enumerate() {
                let Some(here) = &s.mode else { continue };
                let Some(later) = r.claimed_samples[i..]
                    .iter()
                    .find(|o| o.at >= s.at + HORIZON)
                else {
                    continue;
                };
                let Some(there) = &later.mode else { continue };
                let moved = lateral(here, there);
                base_n += 1;
                if moved {
                    base_hit += 1;
                }
                let firing = s.drift.is_some_and(|d| {
                    d.ratio > t && d.coherence >= DRIFT_COHERENCE && d.recent_n >= DRIFT_MIN_RECENT
                });
                if firing {
                    fire_n += 1;
                    if moved {
                        fire_hit += 1;
                    }
                }
            }
        }
        let base = base_hit as f64 / base_n.max(1) as f64;
        let precision = fire_hit as f64 / fire_n.max(1) as f64;
        eprintln!(
            "{t:>7.2} {:>9.1}% {:>10.1}% {fire_n:>10} {:>7.2}",
            base * 100.0,
            precision * 100.0,
            if base > 0.0 {
                precision / base
            } else {
                f64::NAN
            },
        );
    }

    // The two questions PRD §17 asks of any new mark, answered on named
    // sessions rather than in general.
    let focused: Vec<&SessionReport> = reports.iter().filter(|r| r.moves.is_empty()).collect();
    eprintln!("\nsessions whose scope never moved laterally (the false-positive control):");
    for r in &focused {
        let d = &r.detection[&key];
        eprintln!(
            "  {} {:<44} duty {:>5.1}%  {} episode(s)  track {}",
            r.id,
            r.label.chars().take(44).collect::<String>(),
            d.duty * 100.0,
            d.episodes,
            r.claim_track.join(" -> "),
        );
    }
}
