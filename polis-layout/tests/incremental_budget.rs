//! PRD §13.1's incremental layout budget, measured with the machine to itself.
//!
//! | Metric | Budget |
//! |---|---|
//! | Incremental layout step | **< 50 ms, off-thread** |
//!
//! # Why this is its own test target
//!
//! It was in `m1_gate.rs` with twenty-two other tests, and `cargo test` runs the
//! tests inside one binary **in parallel**. Several of those tests generate a
//! five-thousand-file city, so the growth step was being timed on a machine with
//! every core already saturated by the harness that was timing it. Measured on
//! one build, one machine, in the same release profile:
//!
//! | how it was run | median | p95 |
//! |---|---|---|
//! | alone | 36.0 ms | 44.7 ms |
//! | racing the rest of the gate binary | 42.3 ms | 58.8 ms |
//!
//! Those are the same code. The second row is a measurement of the test harness.
//! A budget assertion whose value depends on how many other tests happen to be
//! running is not a budget assertion — it is a flake, and on a two-core runner it
//! is a permanent one.
//!
//! Cargo runs test **targets** one after another, so a target holding a single
//! test gets the machine to itself. That is the whole reason this file exists.
//!
//! **This is not the budget being loosened.** The number asserted here is PRD
//! §13.1's 50 ms, unchanged. What changed is that the step is now measured
//! rather than the harness. The step itself also got faster and, more to the
//! point, *thread-count independent* — see `polis_layout::memo`: measured at
//! 5 000 files, a single add was 44 ms on twenty-four threads and 61 ms on one
//! before that module existed, and is 36 ms on both after it, because the work
//! was removed rather than spread.
//!
//! # What is still recomputed, and what it would take not to
//!
//! A single add moves a median of **four** of 1 165 road nodes, and the step
//! still rebuilds the road graph from the whole cell set: weld, collapse, face
//! walk, prune, betweenness and block assignment are 14 ms of the 36. Localising
//! those needs a **stable block identity**, which the pipeline does not have —
//! `crate::lots`'s cut seed is mixed from the block's *index*, so inserting one
//! face legitimately re-cuts every block after it. That is the next thing to fix
//! here, and it is a layout change rather than a caching one.

use std::time::{Duration, Instant};

use polis_events::LogicalPath;
use polis_layout::city::{self, LayoutInputs};

/// The corpus seed, matching `m1_gate`'s, so the two measure one city.
const GATE_SEED: u64 = 0xACCE_7107_0000_0001;

/// PRD §13.1 budgets a 5 000-file repository's cold start; the incremental step
/// is measured at the same scale.
const FILES: usize = 5_000;

/// Files added one at a time, each timed.
const ADDS: u32 = 12;

fn lp(s: &str) -> LogicalPath {
    LogicalPath::new(s).expect("a valid test path")
}

/// PRD §13.1: an incremental layout step under 50 ms, off-thread.
///
/// Growth is native here — adding a file calls the same `add_file` the batch
/// generator calls — so this measures the real path rather than a shortcut.
#[test]
// `housed` and `shared` are two different questions about the same file, and
// there is no clearer pair of names for them.
#[allow(clippy::similar_names)]
fn a_single_file_add_is_inside_the_incremental_budget() {
    let mut tree = polis_repo::synthetic::repository(FILES, GATE_SEED);
    let mut city = city::generate_city(&tree);
    let inputs = LayoutInputs::default();
    let mut timings = Vec::new();
    let mut moved_nodes = Vec::new();
    let mut reuse = Vec::new();
    let mut shares = 0usize;
    for i in 0..ADDS {
        let path = lp(&format!("core/latecomer{i}.rs"));
        let mut meta = polis_repo::FileMeta::untracked(path.clone(), 3_000 + u64::from(i) * 17);
        meta.growth_index = u32::try_from(tree.files.len()).expect("fits");
        tree.files.insert(path.clone(), meta);
        let started = Instant::now();
        let moved = city.accrete(&tree, &inputs, std::slice::from_ref(&path));
        timings.push(started.elapsed());
        moved_nodes.push(moved);
        reuse.push(city.reuse());
        // The file is **accounted for**: its own lot with a building on it, or
        // an `Overflow` record naming the lot it shares, which the renderer is
        // required to draw. Never neither — a file that vanishes from the map is
        // the one outcome `lots::LotReport` exists to make impossible.
        //
        // Twelve files into one already-surveyed district is the hardest case
        // the incremental path has: they land on the same frontage one at a
        // time, each halving a parcel that the last one already halved. Eleven
        // of the twelve get a lot of their own; the twelfth shares. Asserting
        // "every add gets its own lot" would be asserting that a block can be
        // subdivided without limit.
        let housed = city.building(&path).is_some();
        let shared = city.overflow.iter().any(|o| o.path == path);
        assert!(
            housed || shared,
            "add {i} is on the map nowhere at all: {:?}",
            city.report
        );
        if !housed {
            shares += 1;
        }
        assert_eq!(city.report.components, 1, "add {i} split the city");
    }
    assert!(
        shares <= 1,
        "{shares} of {ADDS} incrementally added files had to share a lot"
    );

    let each: Vec<f64> = timings
        .iter()
        .map(|d| (d.as_secs_f64() * 10_000.0).round() / 10.0)
        .collect();
    timings.sort_unstable();
    moved_nodes.sort_unstable();
    let median = timings[timings.len() / 2];
    let p95 = timings[(timings.len() * 95 / 100).min(timings.len() - 1)];
    let cuts = reuse.iter().map(|r| r.0).sum::<f64>() / f64::from(ADDS);
    let seats = reuse.iter().map(|r| r.1).sum::<f64>() / f64::from(ADDS);
    println!(
        "POLIS_INCREMENTAL median={median:?} p95={p95:?} moved_nodes_median={} of {} \
         reuse=cuts {:.0}% / buildings {:.0}%",
        moved_nodes[moved_nodes.len() / 2],
        city.report.road_nodes,
        cuts * 100.0,
        seats * 100.0,
    );
    println!("POLIS_INCREMENTAL_EACH {each:?} ms");

    // The reuse is asserted, not only reported: a growth step that recomputes
    // the whole city may still come in under the budget on a fast machine and is
    // still the bug PRD §7.4 calls out. This is the assertion that fails if the
    // memo keys stop covering what they key on.
    //
    // The two bounds differ, and the gap is the finding rather than a fudge.
    // Buildings average **87 %** reused: the seat key is the two rings, the spec
    // and the path, all of which are stable. Block cuts average **71 %**,
    // because the cut seed is mixed from the block's *index* — so the four steps
    // of twelve that change the face count re-cut roughly half the city for a
    // reason that has nothing to do with those blocks. Making the cut seed
    // geometric would take that to the building figure; it is a layout change,
    // and it is named in this file's header as the next one.
    assert!(
        cuts > 0.5,
        "a growth step reused only {:.0}% of block cuts; it is re-cutting a city \
         it did not change",
        cuts * 100.0
    );
    assert!(
        seats > 0.75,
        "a growth step reused only {:.0}% of buildings; it is re-seating a city \
         it did not change",
        seats * 100.0
    );

    // PRD §13.1's budget is a property of the profile the product ships, and
    // this test runs in whichever profile the developer chose. Measured on this
    // machine at 5 000 files: release median 36 ms, p95 45 ms, and the same
    // numbers with `RAYON_NUM_THREADS=1`. The test profile is about twice that
    // because the harness itself is unoptimised. The budget is asserted where it
    // means something and reported everywhere.
    let budget = if cfg!(debug_assertions) {
        Duration::from_millis(200)
    } else {
        Duration::from_millis(50)
    };
    assert!(
        p95 < budget,
        "a single-file add took {p95:?}, over the {budget:?} budget for this profile"
    );
}
