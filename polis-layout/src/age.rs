//! The age ramp — real commit time, not position in a file list (PRD §7.1).
//!
//! > **`git log` is the growth order.** Replay it in commit order. Files added
//! > in the repo's **first year** form the old town — dense, tangled, irregular.
//! > Files added last month sit on the periphery and look more planned. (PRD
//! > §7.1)
//!
//! Everything downstream of the settlement reads one number, `t ∈ [0, 1]`: the
//! *age of the ground*. `t = 0` is the historic core (fine mesh, small blocks,
//! tight plot spacing); `t = 1` is the recent periphery (coarse mesh, large
//! planned blocks). `crate::accrete::Params::sep_at` and `cap_at` are the ramp
//! it drives, `crate::roads::choose_prunes` reads it, and PRD §7.1's age
//! structure is what comes out.
//!
//! # What this module exists to fix
//!
//! The ramp used to be **growth-sequence fraction**: a file's index in
//! `git log --diff-filter=A --reverse` divided by the file count. That is a
//! rank, not a time, and it silently asserts that the repository added files at
//! a constant rate. Two very common repositories break it in opposite
//! directions:
//!
//! * A repository whose first year produced 5 % of its files gets an old town of
//!   5 % — one twentieth of the ramp — even though PRD §7.1 says those files
//!   *are* the old town. The historic core disappears.
//! * A repository imported wholesale in one initial commit gets a full,
//!   perfectly smooth core-to-rim gradient out of files that are all **exactly
//!   the same age**. The map draws an age structure that does not exist, and the
//!   operator reads it as fact.
//!
//! Both are cured by ramping on the commit timestamps that [`polis_repo::git`]
//! already carries in [`polis_repo::FileMeta::added_at`].
//!
//! # The ramp
//!
//! Let `first` be the commit time of the oldest file and `last` that of the
//! newest. Then, for a file committed at `at`:
//!
//! ```text
//!               ┌ OLD_TOWN_BAND · (at − first)/YEAR                        at ≤ first+1y
//!   raw(at)  =  │
//!               └ OLD_TOWN_BAND + (1−OLD_TOWN_BAND)·(at − first−YEAR)
//!                                                  ─────────────────      at > first+1y
//!                                                    (last − first−YEAR)
//! ```
//!
//! The first year of history owns the first [`OLD_TOWN_BAND`] of the ramp
//! **whatever fraction of the files it produced**, which is PRD §7.1 read
//! literally. The formula is continuous in the span: as `last − first` falls
//! toward a year the second branch shrinks to nothing, and below a year the
//! first branch is the whole map. There is no cliff at the one-year mark.
//!
//! # The four repositories, and what each degrades to
//!
//! | History | [`AgeRampKind`] | What the city does |
//! |---|---|---|
//! | ≥ 1 year (the design case) | [`Calibrated`](AgeRampKind::Calibrated) | The first year is the old town, at core grain, however many or few files it holds. Everything after spreads over the remaining ramp by real elapsed time. |
//! | > 0 but < 1 year (a young repository) | [`Relative`](AgeRampKind::Relative) | The whole repository is inside its own first year, so by PRD §7.1 it is *all* old town. The absolute ramp would put every file within `OLD_TOWN_BAND·span/YEAR` of zero, so it is stretched to at least [`MIN_SPREAD`] and the reading becomes **relative**: "oldest in this repository", not "older than a year". A month-old repository is a dense town with a small fringe, not a metropolis. |
//! | one commit, or every file added by one initial squash | [`Uniform`](AgeRampKind::Uniform) | There is no age information, so **none is drawn**. Every file sits at [`NO_HISTORY`], the middle of the ramp: one uniform grain, no core, no rim, no gradient. The alternative — falling back to list position — would invent a gradient out of `git log`'s within-commit path order and the operator would read alphabetical order as history. |
//! | a decade, with a wholesale import at the root | [`Calibrated`](AgeRampKind::Calibrated) | The imported files are all *genuinely* the same age, so they all land at `t = 0` and the old town is correspondingly large and fine-grained. This is the truth about that repository and it is what makes an imported tree look imported. It is also the case that costs the most: see [`AgeRamp::old_town`], which the report prints so a large core is visible as a number and not only as a slow generation. |
//!
//! # Determinism
//!
//! Three properties, all required by PRD §7.4:
//!
//! 1. **No clock.** `first` and `last` come from git, never from `now`. A
//!    repository generates the same city today and in ten years.
//! 2. **Monotone in growth index.** `git log --reverse` is chronological except
//!    across a merge, where a commit's date can precede its predecessor's. The
//!    table applies a running maximum, so a file that arrives out of order is
//!    read at its predecessor's time. That keeps "lowest growth index" and
//!    "oldest ground" the same statement, which `crate::accrete` and
//!    `crate::territory` both rely on.
//! 3. **Quantised.** Every value is snapped to a [`RAMP_STEPS`] grid before it
//!    leaves this module, so the `powf` in `sep_at` sees one of a few thousand
//!    inputs rather than an arbitrary `f64`, and a one-second difference in a
//!    commit date cannot move a building.
//!
//! # The ramp is frozen at generation
//!
//! [`AgeRamp`] is built once, from the files the city was generated from, and
//! never rebuilt by an incremental growth step. A new file reads at
//! [`AgeRamp::newest`]. Recalibrating on every add would move `last`, which
//! moves every file's `t`, which moves every plot — precisely the ground-moving
//! PRD §7.7 forbids. The ramp is recalibrated only on a full regeneration.

// Every cast here is a Unix millisecond count or a small file index becoming an
// `f64`. A millisecond timestamp in this century is about 1.8e12 and `f64`
// represents every integer below 2^53 ≈ 9.0e15 exactly, so the conversion is
// lossless for any date between 285 000 BC and 285 000 AD; and the result is
// quantised to `RAMP_STEPS` before it leaves the module in any case.
#![allow(clippy::cast_precision_loss)]

use polis_events::WallTime;
use serde::Serialize;

use crate::determinism::quantize_f64;

/// Milliseconds in a year, the Gregorian mean of 365.2425 days.
///
/// A constant, not a duration derived from a calendar crate: PRD §7.1's "first
/// year" has to mean the same number of milliseconds on every machine and in
/// every release.
pub const YEAR_MS: i64 = 31_556_952_000;

/// The share of the ramp the repository's **first year** owns.
///
/// PRD §7.1 gives the first year the old town, and the old town is the fine end
/// of the grain ramp. `0.40` is chosen so a decade-old repository still spends
/// most of its ramp on the nine years after the founding, while a repository
/// that produced 5 % of its files in year one still gets a real, visible core
/// rather than a knot of five buildings.
pub const OLD_TOWN_BAND: f64 = 0.40;

/// The narrowest ramp a repository with *any* age variation is allowed.
///
/// Below about ten months of history the absolute ramp compresses every file
/// into the first few percent, and the city comes out at one uniform core
/// grain — dense enough at 5 000 files to threaten PRD §13.1's cold-start
/// budget, and flat enough to show nothing. Stretching the observed range to
/// `MIN_SPREAD` keeps the *ordering* (which is real) and gives up the *absolute
/// calibration* (which a young repository cannot support anyway).
pub const MIN_SPREAD: f64 = 0.35;

/// Where a repository with no age information at all sits on the ramp.
///
/// The middle: neither an old town nor a periphery, because it is neither.
pub const NO_HISTORY: f64 = 0.5;

/// Steps the ramp is quantised to.
///
/// `1/4096` is finer than any downstream effect and coarse enough that the
/// `powf` in `crate::accrete::Params::sep_at` sees a small, stable set of
/// inputs (PRD §7.4).
pub const RAMP_STEPS: f64 = 4096.0;

/// Which regime the ramp is in, and therefore how to read the map.
///
/// Serialized into [`crate::city::CityReport`] so a golden file pins it: a
/// repository that quietly slid from `Calibrated` to `Uniform` because its
/// history was rewritten is a changed city and should say so.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgeRampKind {
    /// No file-addition spans any time at all: one commit, or one wholesale
    /// import and nothing since. No gradient is drawn.
    #[default]
    Uniform,
    /// Under a year of history. The whole repository is inside its own first
    /// year; core and rim are relative to this repository, not to a year.
    Relative,
    /// A year or more. PRD §7.1's first year owns [`OLD_TOWN_BAND`] of the ramp.
    Calibrated,
}

impl AgeRampKind {
    /// A short label for the metrics line.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Uniform => "uniform",
            Self::Relative => "relative",
            Self::Calibrated => "calibrated",
        }
    }
}

/// Growth index → age of the ground, calibrated on real commit time.
///
/// Keyed on the growth index rather than on the timestamp because that is what
/// the rest of the pipeline already carries: a district's age is the *minimum
/// growth index* of the files in it, a plot's `birth` is the growth index of its
/// district, and both stay small integers that sort and compare exactly.
#[derive(Debug, Clone)]
pub struct AgeRamp {
    /// Growth indices of tracked files, ascending and unique.
    keys: Vec<u32>,
    /// Ramp value at each key: non-decreasing, quantised, in `[0, 1]`.
    vals: Vec<f64>,
    kind: AgeRampKind,
    /// `last − first`, in milliseconds. Zero for [`AgeRampKind::Uniform`].
    span_ms: i64,
    /// Commit time of the oldest tracked file.
    first: WallTime,
    /// Commit time of the newest tracked file.
    last: WallTime,
    /// Files committed within a year of `first` — PRD §7.1's old town, counted.
    old_town: usize,
}

impl Default for AgeRamp {
    /// The empty ramp: no tracked file, so no age information.
    fn default() -> Self {
        Self {
            keys: Vec::new(),
            vals: Vec::new(),
            kind: AgeRampKind::Uniform,
            span_ms: 0,
            first: WallTime::UNIX_EPOCH,
            last: WallTime::UNIX_EPOCH,
            old_town: 0,
        }
    }
}

impl AgeRamp {
    /// Calibrate the ramp on `(growth index, commit time)` for every tracked
    /// file.
    ///
    /// The input may arrive in any order and may contain untracked files
    /// (`growth_index == u32::MAX`), which are skipped: an untracked file has no
    /// commit time and cannot calibrate anything. A repeated index — which
    /// `polis_repo::tree` produces only when a growth sequence overflows `u32` —
    /// keeps its **earliest** time, so the table is a function.
    #[must_use]
    pub fn calibrate(entries: &[(u32, WallTime)]) -> Self {
        // A `BTreeMap` and not a sort-then-dedup: the caller's order must not be
        // able to decide which of two equal keys wins (PRD §7.4).
        let mut by_index: std::collections::BTreeMap<u32, i64> = std::collections::BTreeMap::new();
        for &(index, at) in entries {
            if index == u32::MAX {
                continue;
            }
            let ms = at.unix_millis();
            by_index
                .entry(index)
                .and_modify(|e| *e = (*e).min(ms))
                .or_insert(ms);
        }
        if by_index.is_empty() {
            return Self::default();
        }

        let keys: Vec<u32> = by_index.keys().copied().collect();
        // Running maximum: see the module docs, "Monotone in growth index".
        let mut times: Vec<i64> = Vec::with_capacity(keys.len());
        let mut high = i64::MIN;
        for &ms in by_index.values() {
            high = high.max(ms);
            times.push(high);
        }

        let first_ms = times[0];
        let last_ms = *times.last().expect("non-empty");
        let span_ms = last_ms.saturating_sub(first_ms);
        let first = WallTime::from_unix_millis(first_ms);
        let last = WallTime::from_unix_millis(last_ms);
        let old_town = times
            .iter()
            .filter(|&&ms| ms.saturating_sub(first_ms) <= YEAR_MS)
            .count();

        if span_ms <= 0 {
            // Every file is the same age. Draw no gradient at all.
            return Self {
                vals: vec![quantize_f64(NO_HISTORY); keys.len()],
                keys,
                kind: AgeRampKind::Uniform,
                span_ms: 0,
                first,
                last,
                old_town,
            };
        }

        let raw: Vec<f64> = times
            .iter()
            .map(|&ms| absolute(ms.saturating_sub(first_ms), span_ms))
            .collect();
        // `raw` is non-decreasing, so its maximum is its last element.
        let hi = *raw.last().expect("non-empty");
        let (kind, scale) = if span_ms >= YEAR_MS {
            (AgeRampKind::Calibrated, 1.0)
        } else if hi > 0.0 && hi < MIN_SPREAD {
            (AgeRampKind::Relative, MIN_SPREAD / hi)
        } else {
            (AgeRampKind::Relative, 1.0)
        };
        let vals: Vec<f64> = raw
            .iter()
            .map(|v| quantize_ramp((v * scale).clamp(0.0, 1.0)))
            .collect();

        Self {
            keys,
            vals,
            kind,
            span_ms,
            first,
            last,
            old_town,
        }
    }

    /// The age of the ground a file of this growth index founded, in `[0, 1]`.
    ///
    /// An index the table does not hold — an untracked file's `u32::MAX`, or a
    /// file added by a growth step after the ramp was frozen — reads as
    /// [`Self::newest`]: as new as anything the calibration saw, which is what
    /// it is.
    #[must_use]
    pub fn at(&self, growth_index: u32) -> f64 {
        if self.vals.is_empty() {
            return quantize_f64(NO_HISTORY);
        }
        // The largest key ≤ `growth_index`. Below the first key — impossible for
        // a real index, since key 0 is the first file — the oldest value wins.
        let i = self.keys.partition_point(|k| *k <= growth_index);
        if i == 0 {
            self.vals[0]
        } else {
            self.vals[i - 1]
        }
    }

    /// The ramp value of the newest file the calibration saw.
    #[must_use]
    pub fn newest(&self) -> f64 {
        self.vals
            .last()
            .copied()
            .unwrap_or_else(|| quantize_f64(NO_HISTORY))
    }

    /// Which regime the ramp is in.
    #[must_use]
    pub fn kind(&self) -> AgeRampKind {
        self.kind
    }

    /// Span of the file-addition history, in whole days.
    #[must_use]
    pub fn span_days(&self) -> u32 {
        let days = self.span_ms / 86_400_000;
        u32::try_from(days).unwrap_or(u32::MAX)
    }

    /// Files committed within a year of the first one: PRD §7.1's old town.
    #[must_use]
    pub fn old_town(&self) -> usize {
        self.old_town
    }

    /// Tracked files the ramp was calibrated on.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// True when no tracked file calibrated the ramp.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Commit time of the oldest tracked file.
    #[must_use]
    pub fn first(&self) -> WallTime {
        self.first
    }

    /// Commit time of the newest tracked file.
    #[must_use]
    pub fn last(&self) -> WallTime {
        self.last
    }
}

/// The uncalibrated ramp: elapsed milliseconds since the founding to `[0, 1]`.
///
/// See the module docs for the formula. `span_ms` must be positive.
fn absolute(elapsed_ms: i64, span_ms: i64) -> f64 {
    debug_assert!(span_ms > 0, "absolute() needs a positive span");
    let elapsed = elapsed_ms.max(0) as f64;
    let year = YEAR_MS as f64;
    if span_ms <= YEAR_MS || elapsed <= year {
        // Inside the first year — including the case where the whole history is.
        return (OLD_TOWN_BAND * elapsed / year).clamp(0.0, 1.0);
    }
    let after = elapsed - year;
    let tail = (span_ms - YEAR_MS) as f64;
    (OLD_TOWN_BAND + (1.0 - OLD_TOWN_BAND) * after / tail).clamp(0.0, 1.0)
}

/// Snap to the [`RAMP_STEPS`] grid.
///
/// `round()` on a value already inside `[0, 1]` is exact, so this is the same
/// number on every target (PRD §7.4).
fn quantize_ramp(t: f64) -> f64 {
    (t.clamp(0.0, 1.0) * RAMP_STEPS).round() / RAMP_STEPS
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;

    /// A commit time for the crate's test corpora: `count` files spread evenly
    /// over the span at which the ramp reproduces growth-index fraction exactly.
    ///
    /// That span is `YEAR / OLD_TOWN_BAND` = 2.5 years: the first year is then
    /// `OLD_TOWN_BAND` of the history as well as `OLD_TOWN_BAND` of the ramp,
    /// and both branches of [`absolute`] collapse to the identity. It keeps
    /// every test written against the old rank-based ramp meaningful while still
    /// exercising the real code path, and it is the *only* span with that
    /// property — which is why it lives here as one shared helper rather than as
    /// a magic number in each corpus builder.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    pub(crate) fn even_history(index: usize, count: usize) -> WallTime {
        const BASE_MS: i64 = 1_500_000_000_000;
        let span = (YEAR_MS as f64) / OLD_TOWN_BAND;
        let last = count.saturating_sub(1).max(1) as f64;
        WallTime::from_unix_millis(BASE_MS + (index as f64 * span / last) as i64)
    }

    fn at_day(day: i64) -> WallTime {
        WallTime::from_unix_millis(1_600_000_000_000 + day * DAY)
    }

    /// A repository whose first year produced 5 % of its files still gets an old
    /// town — the defect this module exists to fix.
    #[test]
    fn a_slow_first_year_still_owns_the_core() {
        let mut entries = Vec::new();
        // Five files in year one, ninety-five in years two to five.
        for i in 0..5u32 {
            entries.push((i, at_day(i64::from(i) * 60)));
        }
        for i in 5..100u32 {
            entries.push((i, at_day(400 + i64::from(i) * 15)));
        }
        let ramp = AgeRamp::calibrate(&entries);
        assert_eq!(ramp.kind(), AgeRampKind::Calibrated);
        assert_eq!(ramp.old_town(), 5);
        // Every year-one file is inside the old-town band.
        for i in 0..5u32 {
            assert!(
                ramp.at(i) <= OLD_TOWN_BAND + 1e-9,
                "file {i} at {} left the core",
                ramp.at(i)
            );
        }
        // Growth-index fraction would have put file 4 at 0.04 of the ramp; real
        // commit time puts it near the top of the core band.
        assert!(ramp.at(4) > 0.15, "the core is not one twentieth wide");
        assert!(ramp.at(99) > 0.99);
    }

    /// The opposite failure: a repository imported in one commit must not be
    /// given a gradient it does not have.
    #[test]
    fn a_wholesale_import_gets_no_invented_gradient() {
        let entries: Vec<(u32, WallTime)> = (0..500u32).map(|i| (i, at_day(0))).collect();
        let ramp = AgeRamp::calibrate(&entries);
        assert_eq!(ramp.kind(), AgeRampKind::Uniform);
        assert_eq!(ramp.span_days(), 0);
        let first = ramp.at(0);
        for i in 0..500u32 {
            assert!(
                (ramp.at(i) - first).abs() < f64::EPSILON,
                "the ramp invented a gradient at {i}"
            );
        }
        assert!((first - quantize_f64(NO_HISTORY)).abs() < 1e-9);
    }

    /// An import followed by years of work: the imported files really are all
    /// the same age, so they really do all sit in the core.
    #[test]
    fn an_import_plus_history_puts_the_import_in_the_core() {
        let mut entries: Vec<(u32, WallTime)> = (0..300u32).map(|i| (i, at_day(0))).collect();
        for i in 300..400u32 {
            entries.push((i, at_day(400 + i64::from(i - 300) * 10)));
        }
        let ramp = AgeRamp::calibrate(&entries);
        assert_eq!(ramp.kind(), AgeRampKind::Calibrated);
        assert_eq!(ramp.old_town(), 300);
        for i in 0..300u32 {
            assert!(ramp.at(i) < 1e-9, "an imported file left the core");
        }
        assert!(ramp.at(399) > 0.99);
    }

    /// Under a year: the ramp is relative, and wide enough to be a city.
    #[test]
    fn a_young_repository_gets_a_relative_ramp() {
        let entries: Vec<(u32, WallTime)> = (0..60u32).map(|i| (i, at_day(i64::from(i)))).collect();
        let ramp = AgeRamp::calibrate(&entries);
        assert_eq!(ramp.kind(), AgeRampKind::Relative);
        assert_eq!(ramp.span_days(), 59);
        assert_eq!(ramp.old_town(), 60);
        assert!(ramp.at(0) < 1e-9);
        assert!(
            (ramp.newest() - MIN_SPREAD).abs() < 2.0 / RAMP_STEPS,
            "expected the stretch to {MIN_SPREAD}, got {}",
            ramp.newest()
        );
    }

    /// The ramp is continuous in the span: no cliff at the one-year mark.
    #[test]
    fn the_one_year_mark_is_not_a_cliff() {
        let just_under = AgeRamp::calibrate(
            &(0..=364u32)
                .map(|i| (i, at_day(i64::from(i))))
                .collect::<Vec<_>>(),
        );
        let just_over = AgeRamp::calibrate(
            &(0..=366u32)
                .map(|i| (i, at_day(i64::from(i))))
                .collect::<Vec<_>>(),
        );
        // Both are stretched or capped near the band; neither jumps to 1.0 and
        // back. The 364-day repository is stretched to MIN_SPREAD; the 366-day
        // one runs the full ramp but spends OLD_TOWN_BAND of it on the first
        // year, which is 364/366 of its files.
        assert_eq!(just_under.kind(), AgeRampKind::Relative);
        assert_eq!(just_over.kind(), AgeRampKind::Calibrated);
        assert_eq!(just_over.old_town(), 366);
        // Only the last day's file is outside the first year, so all but one
        // file of the 366-day repository sits inside the old-town band.
        let outside = (0..=366u32)
            .filter(|&i| just_over.at(i) > OLD_TOWN_BAND + 1e-9)
            .count();
        assert!(outside <= 1, "{outside} files jumped past the core band");
    }

    /// A decade of history spreads the nine years after the founding over the
    /// rest of the ramp.
    #[test]
    fn a_decade_spends_most_of_the_ramp_after_year_one() {
        let entries: Vec<(u32, WallTime)> = (0..120u32)
            .map(|i| (i, at_day(i64::from(i) * 30)))
            .collect();
        let ramp = AgeRamp::calibrate(&entries);
        assert_eq!(ramp.kind(), AgeRampKind::Calibrated);
        // 30-day steps: files 0..=12 are inside the first year.
        assert_eq!(ramp.old_town(), 13);
        assert!(ramp.at(12) <= OLD_TOWN_BAND + 1e-9);
        assert!(ramp.at(13) > OLD_TOWN_BAND);
        assert!(ramp.at(119) > 0.99);
    }

    /// A commit date that goes backwards across a merge must not make the ramp
    /// non-monotone, or "oldest index" would stop meaning "oldest ground".
    #[test]
    fn an_out_of_order_commit_date_cannot_unsort_the_ramp() {
        let entries = vec![
            (0u32, at_day(0)),
            (1, at_day(400)),
            (2, at_day(200)), // a merge brought an older commit in later
            (3, at_day(800)),
        ];
        let ramp = AgeRamp::calibrate(&entries);
        assert!(ramp.at(0) <= ramp.at(1));
        assert!(ramp.at(1) <= ramp.at(2));
        assert!(ramp.at(2) <= ramp.at(3));
        // The out-of-order file reads at its predecessor's time.
        assert!((ramp.at(2) - ramp.at(1)).abs() < f64::EPSILON);
    }

    /// Untracked files calibrate nothing and read as the newest ground.
    #[test]
    fn untracked_files_read_as_the_newest_ground() {
        let entries = vec![
            (0u32, at_day(0)),
            (1, at_day(500)),
            (u32::MAX, WallTime::UNIX_EPOCH),
        ];
        let ramp = AgeRamp::calibrate(&entries);
        assert_eq!(ramp.len(), 2);
        assert!((ramp.at(u32::MAX) - ramp.newest()).abs() < f64::EPSILON);
        assert!((ramp.at(99) - ramp.newest()).abs() < f64::EPSILON);
    }

    /// No tracked file at all: a checkout with no git history.
    #[test]
    fn an_untracked_checkout_has_no_ramp() {
        let ramp = AgeRamp::calibrate(&[(u32::MAX, WallTime::UNIX_EPOCH)]);
        assert!(ramp.is_empty());
        assert_eq!(ramp.kind(), AgeRampKind::Uniform);
        assert!((ramp.at(0) - quantize_f64(NO_HISTORY)).abs() < 1e-9);
    }

    /// The caller's order cannot reach the table (PRD §7.4).
    #[test]
    fn input_order_cannot_move_the_ramp() {
        let forward: Vec<(u32, WallTime)> =
            (0..50u32).map(|i| (i, at_day(i64::from(i) * 20))).collect();
        let mut shuffled = forward.clone();
        shuffled.reverse();
        shuffled.swap(3, 41);
        let a = AgeRamp::calibrate(&forward);
        let b = AgeRamp::calibrate(&shuffled);
        assert_eq!(a.keys, b.keys);
        assert_eq!(a.vals, b.vals);
        assert_eq!(a.kind, b.kind);
    }

    /// Every value is on the quantisation grid, so `sep_at`'s `powf` sees a
    /// small stable set of inputs.
    #[test]
    fn every_value_is_quantised() {
        let entries: Vec<(u32, WallTime)> = (0..200u32)
            .map(|i| {
                (
                    i,
                    WallTime::from_unix_millis(1_600_000_000_000 + i64::from(i) * 997_003),
                )
            })
            .collect();
        let ramp = AgeRamp::calibrate(&entries);
        for i in 0..200u32 {
            let v = ramp.at(i);
            let steps = v * RAMP_STEPS;
            assert!(
                (steps - steps.round()).abs() < 1e-9,
                "value {v} is off the grid"
            );
        }
    }

    /// The test-corpus history reproduces growth-index fraction, which is what
    /// lets every test written against the old ramp keep its meaning.
    #[test]
    fn the_test_corpus_history_is_the_identity_ramp() {
        const N: usize = 41;
        let entries: Vec<(u32, WallTime)> = (0..N)
            .map(|i| (u32::try_from(i).expect("small"), even_history(i, N)))
            .collect();
        let ramp = AgeRamp::calibrate(&entries);
        assert_eq!(ramp.kind(), AgeRampKind::Calibrated);
        for i in 0..N {
            let want = i as f64 / (N - 1) as f64;
            let got = ramp.at(u32::try_from(i).expect("small"));
            assert!(
                (got - want).abs() < 4.0 / RAMP_STEPS,
                "file {i}: ramp {got} is not the index fraction {want}"
            );
        }
    }

    /// A one-second difference in a commit date cannot move a building.
    #[test]
    fn a_one_second_jitter_does_not_move_the_ramp() {
        let base: Vec<(u32, WallTime)> =
            (0..400u32).map(|i| (i, at_day(i64::from(i) * 5))).collect();
        let jittered: Vec<(u32, WallTime)> = base
            .iter()
            .map(|&(i, at)| (i, WallTime::from_unix_millis(at.unix_millis() + 1_000)))
            .collect();
        let a = AgeRamp::calibrate(&base);
        let b = AgeRamp::calibrate(&jittered);
        assert_eq!(a.vals, b.vals, "a second of jitter moved the ramp");
    }
}
