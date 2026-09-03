//! Evidence for PRD §17's **open question 2**, and for how long a pin stands.
//!
//! > 2. Should `done, unverified` be a fourth attention state rather than a
//! >    variant of `done`? It behaves more like `needs decision`.
//!
//! The question cannot be answered from the PRD, because it is a question about
//! *frequency*: a fourth state is worth its cost only if the variant is common
//! enough and long-lived enough to need its own rank, its own shape and its own
//! place in §11.1's ordering. So this replays the operator's real sessions and
//! counts.
//!
//! It also measures the one number PRD §11.4 needs and does not state: how long
//! a "needs decision" mark actually stands before it is resolved. §11.4 fixes
//! the arrival pulse at ≤400 ms and says the steady state is "shape and
//! position" — but a pin that has stood for twenty minutes is a different fact
//! from one that arrived a second ago, and the escalation window has to come
//! from the corpus rather than from taste. See
//! `polis_world::attention::ESCALATION`.
//!
//! Ignored by default: it reads `~/.claude/projects`.
//!
//! ```sh
//! cargo test --release -p polis-world --test attention_states -- --ignored --nocapture
//! ```

#![allow(clippy::cast_precision_loss)]

use std::collections::BTreeSet;
use std::time::Instant;

use polis_events::PathMapper;
use polis_layout::CityLayout;
use polis_world::attention::{AttentionKind, DecisionSource};
use polis_world::replay::{ReplayDriver, DEFAULT_IDLE_GAP_CAP};
use polis_world::sessions::{IndexOptions, SessionIndex};
use polis_world::{DenylistUbiquity, World};

/// How many sample points across each session's compressed timeline.
const SAMPLES: usize = 240;

/// How many of the largest sessions to replay.
const SESSIONS: usize = 6;

#[derive(Default)]
struct Tally {
    /// Sample points at which at least one mark of this kind stood.
    frames: [usize; 4],
    /// The most that stood at once.
    peak: [usize; 4],
    /// Distinct raises, keyed by (thread, arrival instant).
    raises: [BTreeSet<(String, Instant)>; 4],
    /// Standing ages of "needs decision" marks, in seconds, one per sample.
    waits: Vec<f64>,
    /// Which sources raised them.
    sources: Vec<(DecisionSource, usize)>,
    /// Per sample, per thread that has written a file: would a `Stop` arriving
    /// at this instant raise `done, unverified`?
    would_be_unverified: usize,
    would_be_verified: usize,
    samples: usize,
}

const NAMES: [&str; 4] = [
    "contention",
    "needs-decision",
    "done-unverified",
    "done-verified",
];

/// The slot a mark occupies in the tallies — the ordering PRD §11.1 asks for,
/// with `done` split by verification because that split is what §17's open
/// question 2 is about.
fn slot(kind: &AttentionKind) -> usize {
    match kind {
        AttentionKind::Contention(_) => 0,
        AttentionKind::NeedsDecision { .. } => 1,
        AttentionKind::Done {
            verified: false, ..
        } => 2,
        AttentionKind::Done { verified: true, .. } => 3,
    }
}

impl Tally {
    fn observe(&mut self, world: &World, now: Instant) {
        self.samples += 1;
        let mut seen = [0usize; 4];
        for mark in &world.attention {
            let s = slot(&mark.kind);
            seen[s] += 1;
            self.raises[s].insert((mark.thread().to_string(), mark.since));
            if let AttentionKind::NeedsDecision { source, .. } = &mark.kind {
                self.waits
                    .push(now.saturating_duration_since(mark.since).as_secs_f64());
                if let Some(e) = self.sources.iter_mut().find(|(s, _)| s == source) {
                    e.1 += 1;
                } else {
                    self.sources.push((*source, 1));
                }
            }
        }
        for (i, n) in seen.into_iter().enumerate() {
            if n > 0 {
                self.frames[i] += 1;
            }
            self.peak[i] = self.peak[i].max(n);
        }
        // PRD §17 q2 needs a rate, and a replay can never raise a `done` mark at
        // all (`Stop` is Channel B — see the module docs). So the question is
        // asked counterfactually at every sample, of exactly the predicate
        // `finish_thread` reads: if this thread stopped now, which variant?
        for thread in world.threads.values() {
            if thread.visits.values().all(|v| v.writes == 0) {
                continue;
            }
            if thread.is_verified(&world.files) {
                self.would_be_verified += 1;
            } else {
                self.would_be_unverified += 1;
            }
        }
    }

    fn merge(&mut self, other: &Self) {
        for i in 0..4 {
            self.frames[i] += other.frames[i];
            self.peak[i] = self.peak[i].max(other.peak[i]);
            for r in &other.raises[i] {
                self.raises[i].insert(r.clone());
            }
        }
        self.would_be_unverified += other.would_be_unverified;
        self.would_be_verified += other.would_be_verified;
        self.waits.extend_from_slice(&other.waits);
        for (s, n) in &other.sources {
            if let Some(e) = self.sources.iter_mut().find(|(k, _)| k == s) {
                e.1 += n;
            } else {
                self.sources.push((*s, *n));
            }
        }
        self.samples += other.samples;
    }

    fn report(&self, label: &str) {
        eprintln!("  [{label}] {} samples", self.samples);
        for (i, name) in NAMES.into_iter().enumerate() {
            eprintln!(
                "    {name:<16} standing in {:>5.1}% of samples, peak {:>2} at once,                  {:>3} distinct raises",
                100.0 * self.frames[i] as f64 / self.samples.max(1) as f64,
                self.peak[i],
                self.raises[i].len(),
            );
        }
        if !self.waits.is_empty() {
            let mut w = self.waits.clone();
            w.sort_by(f64::total_cmp);
            let over = |limit: f64| {
                100.0 * w.iter().filter(|v| **v >= limit).count() as f64 / w.len() as f64
            };
            eprintln!(
                "    pin standing age: p50 {:.0}s, p75 {:.0}s, p90 {:.0}s, max {:.0}s                  ({} observations); {:.0}% over 60s, {:.0}% over 300s, {:.0}% over 900s",
                w[w.len() / 2],
                w[w.len() * 3 / 4],
                w[w.len() * 9 / 10],
                w[w.len() - 1],
                w.len(),
                over(60.0),
                over(300.0),
                over(900.0),
            );
        }
        let stops = self.would_be_unverified + self.would_be_verified;
        if stops > 0 {
            eprintln!(
                "    a `Stop` arriving at a random instant raises done-UNVERIFIED {:.1}% of the                  time ({} of {} thread-samples that had written a file)",
                100.0 * self.would_be_unverified as f64 / stops as f64,
                self.would_be_unverified,
                stops,
            );
        }
        let mut sources = self.sources.clone();
        sources.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        for (s, n) in sources {
            eprintln!("      source {:<16} {n}", s.label());
        }
    }
}

#[test]
#[ignore = "reads the operator's real sessions"]
fn how_often_each_attention_state_actually_stands() {
    let Some(projects) = polis_world::sessions::default_projects_dir().filter(|p| p.is_dir())
    else {
        eprintln!("skipped: no ~/.claude/projects on this machine");
        return;
    };
    let index = SessionIndex::scan_with(&projects, &IndexOptions::quick()).expect("scan");
    let mut candidates: Vec<_> = index
        .sessions
        .iter()
        .filter(|s| s.repo_exists && s.repo.is_some())
        .collect();
    candidates.sort_by_key(|s| std::cmp::Reverse(s.bytes));
    candidates.truncate(SESSIONS);
    assert!(!candidates.is_empty(), "no replayable sessions on disk");

    let mut total = Tally::default();
    let mut ended_unverified = 0usize;
    let mut ended_verified = 0usize;
    for sample in candidates {
        let repo_path = sample.repo.clone().expect("a repo");
        let Ok(mapper) = PathMapper::new(&repo_path) else {
            continue;
        };
        let Ok(mut schedule) = sample.schedule(&mapper) else {
            continue;
        };
        if schedule.is_empty() {
            continue;
        }
        schedule.compress_idle_gaps(Some(DEFAULT_IDLE_GAP_CAP));
        let mut world = World::for_replay(CityLayout::default());
        world.set_ubiquity(Box::new(DenylistUbiquity::default()));
        let origin = Instant::now();
        let mut driver = ReplayDriver::with_origin(schedule.clone(), origin);
        let mut tally = Tally::default();
        let span = schedule.duration_ms().max(1);
        for i in 0..SAMPLES {
            let at = span * i as u64 / (SAMPLES as u64 - 1);
            driver.seek(at, &mut world);
            let now = origin + std::time::Duration::from_millis(schedule.session_ms_at(at));
            tally.observe(&world, now);
        }
        // PRD §17 q2 needs the *rate*, and a transcript replay can never raise a
        // `done` mark at all — `Stop` is Channel B (see the module docs). So the
        // verification state each thread would have finished in is read straight
        // off the world, which is exactly what `finish_thread` reads.
        for thread in world.threads.values() {
            if thread.visits.values().all(|v| v.writes == 0) {
                continue;
            }
            if thread.is_verified(&world.files) {
                ended_verified += 1;
            } else {
                ended_unverified += 1;
            }
        }
        tally.report(&format!(
            "{} · {} events",
            &sample.session.as_str()[..8],
            schedule.len()
        ));
        total.merge(&tally);
    }
    eprintln!("---");
    total.report("ALL");
    eprintln!(
        "  threads that wrote a file and then stopped: {ended_unverified} would raise          done-UNVERIFIED, {ended_verified} done-verified"
    );
}
