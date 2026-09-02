//! What the layout phase costs as a repository gets bigger, measured rather
//! than asserted.
//!
//! # The claim this test exists to settle
//!
//! `accrete::grow` was reported as "super-linear in file count — 912 ms of
//! Django's 1 129 ms layout phase, and this is what will hurt first on
//! repositories larger than Django". The first half is true and the second half
//! is a misreading of it. Measured on the synthetic family, where the shape of
//! the directory tree is held constant and only the file count moves:
//!
//! | files | plots | layout | ms per file | ms per plot |
//! |---|---|---|---|---|
//! |  1 000 |   351 |    92 ms | 0.092 | 0.261 |
//! |  2 000 |   711 |   202 ms | 0.101 | 0.284 |
//! |  4 000 |   841 |   254 ms | 0.064 | 0.302 |
//! |  8 000 | 1 596 |   471 ms | 0.059 | 0.295 |
//! | 16 000 | 2 966 |   927 ms | 0.058 | 0.313 |
//! | 32 000 | 5 383 | 1 976 ms | 0.062 | 0.367 |
//!
//! Eight times the files from 4 000 to 32 000 costs **7.8 times** the layout.
//! The log-log slope over the four sizes this test measures is **0.75**, and it
//! is 0.99 over the top three. The phase is linear, and it is linear in the
//! **plot** count rather than the file count: milliseconds per plot sits between
//! 0.26 and 0.37 across an eight-fold range and across four real repositories.
//!
//! # So why is Django twice the cost per file?
//!
//! Because a plot is settled per *directory* as well as per capacity-full of
//! files, and the four real corpora differ by an order of magnitude in how many
//! directories they have per file:
//!
//! | corpus | files | directories | plots | layout | ms/file | ms/plot |
//! |---|---|---|---|---|---|---|
//! | Neovim  | 3 890 |   174 |   849 |   258 ms | 0.066 | 0.304 |
//! | `CPython` | 6 138 |   415 | 1 367 |   442 ms | 0.072 | 0.323 |
//! | Ansible | 5 789 | 1 942 | 2 432 |   922 ms | 0.159 | 0.379 |
//! | Django  | 7 014 | 2 037 | 2 505 | 1 026 ms | 0.146 | 0.410 |
//!
//! Milliseconds per file spans 2.7x and tracks directories-per-file almost
//! exactly (0.045, 0.068, 0.34, 0.29). Milliseconds per plot spans 1.35x. The
//! cost model is `plots ≈ directories + files / capacity`, and Django is dear
//! because it has 2 037 directories, not because it has 7 014 files.
//!
//! **What that means for a repository larger than Django**: the term to watch is
//! the directory count, and the growth is linear in it. A 50 000-file repository
//! with Neovim's tree shape costs about what 32 000 synthetic files cost.
//!
//! # The constant factor that is genuinely wrong, and where it is
//!
//! Profiled at Django's scale before this test existed: of 928 ms in
//! `accrete::grow`, **862 ms** is inside `Settlement::try_hosts`, which calls
//! `Settlement::evaluate` 4 430 098 times — about 1 790 candidate positions per
//! settled plot — and each of those does one spatial-hash scan. Those scans
//! walked **136 million** plot entries and probed **88 million** grid cells for
//! a city of 2 472 plots, an average of 28.8 plots visited per scan where only
//! about four are inside the query window.
//!
//! The cause is one line: the hash is built at `Grid::new(params.sep_rim *
//! 1.25)` — a cell 3.44 units across — while the historic core packs plots at
//! `params.sep_core = 0.55` and queries it at a window of `0.55 · 1.9`. One
//! coarse cell holds ~30 core plots, so every query in the old town over-scans
//! by an order of magnitude, and it gets *worse* the deeper the history is,
//! because a longer ramp makes a finer core. That is a 2-3x constant on the
//! growth stage, and it is not asymptotic: the scaling above already contains
//! it.
//!
//! Fixing it is not a matter of shrinking the cell. `Settlement::evaluate`'s
//! `nearest_same` is a minimum over *everything the scan visits*, not over the
//! query window — so the set of cells scanned is load-bearing on the output, and
//! changing the cell size changes the city. The fix that preserves the city
//! exactly is a two-level grid (a fine one for core-grain queries, a coarse one
//! for rim-grain queries, both holding every plot) or a dense array in place of
//! the open-addressed cell map, which removes the 88 million hash probes without
//! touching which plots are visited or in what order.

use std::time::Instant;

use polis_layout::city;

/// The corpus seed, matching `m1_gate`'s and `incremental_budget`'s.
const GATE_SEED: u64 = 0xACCE_7107_0000_0001;

/// Four sizes, each double the last, so a slope is a slope and not two points.
const SIZES: [usize; 4] = [2_000, 4_000, 8_000, 16_000];

/// One measured point.
struct Point {
    files: usize,
    plots: usize,
    millis: f64,
}

/// The layout phase is linear in the settled ground, over an eight-fold range.
#[test]
#[allow(clippy::cast_precision_loss)] // counts becoming a ratio
fn the_layout_phase_scales_linearly() {
    let mut points = Vec::new();
    for files in SIZES {
        let tree = polis_repo::synthetic::repository(files, GATE_SEED);
        let started = Instant::now();
        let city = city::generate_city(&tree);
        let millis = started.elapsed().as_secs_f64() * 1_000.0;
        points.push(Point {
            files,
            plots: city.report.plots,
            millis,
        });
    }

    for p in &points {
        println!(
            "POLIS_SCALING files={} plots={} layout={:.1}ms per_file={:.3} per_plot={:.3}",
            p.files,
            p.plots,
            p.millis,
            p.millis / p.files as f64,
            p.millis / p.plots as f64,
        );
    }

    // Least-squares slope of log(milliseconds) against log(files). One is
    // linear; two would be quadratic.
    let xs: Vec<f64> = points.iter().map(|p| (p.files as f64).ln()).collect();
    let ys: Vec<f64> = points.iter().map(|p| p.millis.ln()).collect();
    let mean_x = xs.iter().sum::<f64>() / xs.len() as f64;
    let mean_y = ys.iter().sum::<f64>() / ys.len() as f64;
    let cov: f64 = xs
        .iter()
        .zip(&ys)
        .map(|(x, y)| (x - mean_x) * (y - mean_y))
        .sum();
    let var: f64 = xs.iter().map(|x| (x - mean_x).powi(2)).sum();
    let slope = cov / var;

    let per_plot: Vec<f64> = points.iter().map(|p| p.millis / p.plots as f64).collect();
    let lo = per_plot.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = per_plot.iter().copied().fold(0.0f64, f64::max);
    println!(
        "POLIS_SCALING slope={slope:.2} (1.0 is linear) per_plot_spread={:.2}x",
        hi / lo
    );

    // Measured 0.75 on this machine, 0.99 over the top three sizes. The bound is
    // set at 1.30 rather than at the measurement: this is a *timing* test on a
    // machine that may be shared, and the failure it exists to catch is a
    // genuinely super-linear term — an O(n) neighbour scan per file, a frontier
    // re-sort per placement, a containment test against every block — which
    // would put the slope past 1.5 long before it reached 1.3.
    assert!(
        slope < 1.30,
        "the layout phase scales as files^{slope:.2}; it is meant to be linear"
    );

    // The stronger statement, and the one that says *what* it is linear in.
    // Measured spread 1.10x across these four sizes and 1.35x across the four
    // real repositories; bounded at 2.0 so machine noise cannot fail it.
    assert!(
        hi / lo < 2.0,
        "milliseconds per plot ranged {lo:.3} to {hi:.3} ({:.1}x) across the four \
         sizes; the growth is supposed to cost a constant per plot",
        hi / lo
    );

    // And the plot count itself must not run away with the file count, or
    // "linear per plot" would be no comfort at all.
    let first = &points[0];
    let last = &points[points.len() - 1];
    let files_ratio = last.files as f64 / first.files as f64;
    let plots_ratio = last.plots as f64 / first.plots as f64;
    assert!(
        plots_ratio < files_ratio * 1.2,
        "{files_ratio:.1}x the files settled {plots_ratio:.1}x the plots"
    );
}
