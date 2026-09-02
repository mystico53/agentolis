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
//! every core already saturated by the harness that was timing it. A budget
//! assertion whose value depends on how many other tests happen to be running is
//! not a budget assertion — it is a flake, and on a two-core runner it is a
//! permanent one.
//!
//! Cargo runs test **targets** one after another, so a target holding a single
//! test gets the machine to itself. That is the whole reason this file exists.
//!
//! **This is not the budget being loosened.** The number asserted here is PRD
//! §13.1's 50 ms, unchanged.
//!
//! # Where the step stands, measured
//!
//! One machine, one build, `--release`, 5 000 files, twelve single-file adds,
//! `RAYON_NUM_THREADS` forced so the contention case is measured rather than
//! assumed. `p95` over twelve samples is the worst of them, which is the honest
//! statistic for a budget. The two columns are the same tree with this file's
//! three fixes reverted and applied, so nothing else moved between them:
//!
//! | rayon threads | before (median / p95) | after (median / p95) |
//! |---|---|---|
//! | 1 | 52.8 / 75.5 ms | **36.8 / 39.4 ms** |
//! | 2 | 54.5 / 64.6 ms | **35.4 / 38.6 ms** |
//! | 4 | 52.2 / 59.5 ms | **34.8 / 38.7 ms** |
//! | 24 (all) | 49.0 / 55.7 ms | **34.1 / 36.4 ms** |
//!
//! Re-measured this round at one rayon thread on an idle machine: median
//! 34.3 ms, p95 **41.2 ms**, twelve samples spanning 30.8-41.2 ms. The gate's
//! 57.6 ms was taken at one thread *under load*, which is the case the section
//! below is about.
//!
//! Every row was over the 50 ms budget before, on a machine with nothing else
//! running; every row is now inside it, **including on a single core**, which is
//! the contention case that made the gate yellow. Repeated four times per thread
//! count the p95 ranges 39.9-48.6 ms on one thread and 38.8-43.1 ms on four, so
//! the margin is real rather than one lucky sample.
//!
//! It is still a *timing* measurement on a shared machine, and that cuts both
//! ways: with six `cargo` processes and a hung test binary spinning on this box
//! the same code has produced 102 ms at one thread. A budget assertion cannot be
//! made immune to a machine that is already busy — it can only be given the
//! machine, which is what this file being its own target does. A failure here
//! means "look at the machine first, then the code".
//!
//! Cache reuse over the same twelve adds went from **cuts 47 % / buildings 66 %**
//! to **78 % / 96 %**.
//!
//! # The number that matters more than the clock
//!
//! PRD §7.7: *never move the ground while the operator is looking at it.* This
//! test counts how many buildings' footprints changed on each add, and the
//! before/after is the real result:
//!
//! | | before | after |
//! |---|---|---|
//! | median add | 1 273 of 4 577 (28 %) | **74 (2 %)** |
//! | worst add | 4 326 of 4 577 (**95 %**) | **620 (14 %)** |
//!
//! One added file used to re-place nineteen buildings in twenty. That was never
//! visible as a *correctness* failure, because every rebuild was deterministic
//! and the same repository still produced the same city — but PRD §7.7's promise
//! was being broken on every growth step, and the incremental budget was the
//! symptom that finally showed it.
//!
//! # The step is bimodal, and the reason is worth stating
//!
//! Six of the twelve adds land on a plot that already exists; six settle a new
//! one. The first kind moves 2-6 buildings. The second moves 74-620, and the
//! difference is **not** the new plot's own neighbourhood — it is that
//! `regions::partition` reassigns plots between districts when the plot set
//! grows, `regions::seat_files` then moves files between plots, and every block
//! whose file list changed has to be cut and seated again. Measured on the worst
//! of the twelve: ~200 blocks of ~900 change their file list, against ~49 that
//! change shape.
//!
//! Making the partition incremental is the remaining lever on that **churn**.
//! It is not a caching problem and no cache can fix it, because the arguments
//! really did change. That is PRD §7.7's business rather than PRD §13.1's.
//!
//! # It is NOT the remaining lever on the clock, and this file used to imply it
//!
//! The previous version of this header named `regions::partition` as "the
//! remaining lever" without separating churn from time, and the next reader took
//! it as a performance claim. It is not one. Profiled directly at 5 000 files,
//! release, one stage at a time:
//!
//! | stage | ms | what it is |
//! |---|---:|---|
//! | `Graph::from_cells_indexed` | 2.13 | weld the Voronoi corners into a planar graph |
//! | `regions::adjacency` | 0.15 | |
//! | **`regions::partition`** | **3.41** | the district partition |
//! | `Settlement::reseated` | 0.33 | |
//! | `districts::links_to_keep` | 0.28 | |
//! | `Graph::collapse_short` + `compact_nodes` | 2.68 | |
//! | `Graph::faces` (first pass) | 0.42 | |
//! | `face_districts` + `border_edges` + `choose_prunes` | 3.23 | |
//! | `delete_edges` + `compact_nodes` + `faces` | 0.68 | |
//! | `classify_by_betweenness` | 1.04 | |
//! | `blocks::assign` | 2.58 | |
//! | `blocks::heal_districts` | 0.52 | |
//! | **graph pipeline, total** | **17.4** | redone in full on every step |
//! | `lots::parcel_city` | 30.7 cold / ~9 warm | 78 % of block cuts are cache hits |
//! | building seating | ~30 cold / ~2 warm | 96 % of buildings are cache hits |
//!
//! **The partition is a tenth of the step.** Making it incremental would buy at
//! most 3.4 ms of a 35 ms floor, and the floor is what to attack: it is paid in
//! full even by a step that moves three buildings, because the whole cell
//! diagram is re-welded, re-collapsed, re-pruned and re-faced whether or not a
//! plot was added. Half the adds in this test add no plot at all, and for those
//! every one of the seventeen milliseconds above recomputes an identical answer.
//!
//! That is the real lever, it is a cache keyed on the cell diagram rather than
//! an incremental partition, and it is **not taken here** (ADR-0083): the step is inside
//! its budget (p95 41 ms on one core against 50 ms) and the change spans
//! `city::assemble`, which this round's parallel work owns. Recorded so the next
//! reader attacks the right stage.
//!
//! # What was fixed here, and what it was
//!
//! Two seeds that were derived from an **array index** and therefore re-rolled
//! the whole city whenever anything was inserted before them:
//!
//! * `lots::block_cut_seed` — was `first file's path hash ^ block index * 31`,
//!   so inserting one face re-cut every block after it.
//! * `roads::edge_identity` — the prune coin was seeded from the edge's two
//!   **node indices**, which the weld renumbers, so one added plot re-rolled the
//!   prune for every road in the city. Measured: 242 blocks of ~900 changed
//!   shape, against 49 with the geometric identity.
//!
//! And one cache boundary in the wrong place: `memo::CutCache` remembered the
//! raw subdivision (0.4 ms) and not the parcel geometry around it (7.1 ms on
//! every step, hit or miss). See `polis_layout::memo::BlockCut`.

use std::collections::BTreeMap;
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

/// Every building's footprint, by path, so two steps can be compared.
///
/// Quantised to the same thousandth the golden files use, so "moved" means
/// "moved somewhere the renderer would draw differently" rather than "differs in
/// the last bit of an `f32`".
// The coordinates are city-space, bounded by the extent and multiplied by a
// thousand; the cast cannot overflow an `i64` for any city this pipeline builds.
#[allow(clippy::cast_possible_truncation)]
fn footprints(city: &city::City) -> BTreeMap<LogicalPath, Vec<[i64; 2]>> {
    city.layout
        .buildings
        .iter()
        .map(|(path, b)| {
            let ring = b
                .footprint
                .vertices
                .iter()
                .map(|p| {
                    (
                        (f64::from(p.x) * 1_000.0).round() as i64,
                        (f64::from(p.y) * 1_000.0).round() as i64,
                    )
                })
                .map(|(x, y)| [x, y])
                .collect();
            (path.clone(), ring)
        })
        .collect()
}

/// PRD §13.1: an incremental layout step under 50 ms, off-thread.
///
/// Growth is native here — adding a file calls the same `add_file` the batch
/// generator calls — so this measures the real path rather than a shortcut.
#[test]
// `housed` and `shared` are two different questions about the same file, and
// there is no clearer pair of names for them. The counts becoming percentages
// are counts of one city's parts, and the measurement is one ordered sequence
// that reads worse cut into fragments called once each.
#[allow(
    clippy::similar_names,
    clippy::cast_precision_loss,
    clippy::too_many_lines
)]
fn a_single_file_add_is_inside_the_incremental_budget() {
    let mut tree = polis_repo::synthetic::repository(FILES, GATE_SEED);
    let mut city = city::generate_city(&tree);
    let inputs = LayoutInputs::default();
    let mut timings = Vec::new();
    let mut moved_nodes = Vec::new();
    let mut moved_buildings = Vec::new();
    let mut reuse = Vec::new();
    let mut shares = 0usize;
    let mut before = footprints(&city);
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

        // PRD §7.7's number, not the clock's: how much of the city the operator
        // was looking at moved underneath them. A building counts as moved when
        // its footprint differs at the quantisation the renderer draws at; the
        // file just added is excluded, because it has to arrive somewhere.
        let after = footprints(&city);
        let churn = before
            .iter()
            .filter(|(p, ring)| **p != path && after.get(*p).is_none_or(|now| now != *ring))
            .count();
        moved_buildings.push(churn);
        before = after;

        // The file is **accounted for**: its own lot with a building on it, or
        // an `Overflow` record naming the lot it shares, which the renderer is
        // required to draw. Never neither — a file that vanishes from the map is
        // the one outcome `lots::LotReport` exists to make impossible.
        //
        // Twelve files into one already-surveyed district is the hardest case
        // the incremental path has: they land on the same frontage one at a
        // time, each halving a parcel that the last one already halved.
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
    let churn = moved_buildings.clone();
    timings.sort_unstable();
    moved_nodes.sort_unstable();
    moved_buildings.sort_unstable();
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
    println!(
        "POLIS_INCREMENTAL_CHURN moved_buildings median={} worst={} of {} ({:.0}% / {:.0}%) \
         each={churn:?}",
        moved_buildings[moved_buildings.len() / 2],
        moved_buildings[ADDS as usize - 1],
        city.report.buildings,
        100.0 * moved_buildings[moved_buildings.len() / 2] as f64 / city.report.buildings as f64,
        100.0 * moved_buildings[ADDS as usize - 1] as f64 / city.report.buildings as f64,
    );

    // The reuse is asserted, not only reported: a growth step that recomputes
    // the whole city may still come in under the budget on a fast machine and is
    // still the bug PRD §7.4 calls out. This is the assertion that fails if the
    // memo keys stop covering what they key on.
    //
    // The bounds are set below the measurement, not at it. Measured on this
    // machine at 5 000 files: **cuts 77 %, buildings 90 %**, and the twelve
    // samples split cleanly — a step that only seats a file into an existing
    // plot reuses 99.9 % of both, and a step that settles a new plot reuses
    // 45-95 %, because `regions::partition` moves files between plots. So the
    // floors are set to catch the *cache* breaking (which would take both to
    // near zero), not to pin the partition's behaviour, which this file does not
    // own and which `POLIS_INCREMENTAL_CHURN` reports instead.
    //
    // These numbers replace the 71 % / 87 % this file used to document. Those
    // were measured before `lots::block_cut_seed` stopped being mixed from the
    // block's array index, and the file kept quoting them after the tree had
    // moved on — which is exactly the failure this comment now exists to avoid.
    assert!(
        cuts > 0.65,
        "a growth step reused only {:.0}% of block cuts; it is re-cutting a city \
         it did not change",
        cuts * 100.0
    );
    assert!(
        seats > 0.80,
        "a growth step reused only {:.0}% of buildings; it is re-seating a city \
         it did not change",
        seats * 100.0
    );

    // PRD §7.7, as a number rather than a hope.
    //
    // Both bounds are set where they **fail the state this file was written to
    // fix** and pass what replaced it with room to spare, rather than at
    // whatever today's build happens to measure. Reverting the three fixes named
    // in this file's header gives a median add that moves 28 % of the city and a
    // worst that moves 95 %; with them it is 1-10 % and 14-42 %, the range
    // depending on how much `regions::partition` moves on the day. A quarter and
    // three fifths sit between those, so this cannot pass a pipeline that
    // rebuilds the city to add one file, and it cannot flake on one that does
    // not.
    let median_churn = moved_buildings[moved_buildings.len() / 2];
    let worst = moved_buildings[ADDS as usize - 1];
    assert!(
        median_churn * 4 < city.report.buildings,
        "the median add moved {median_churn} of {} buildings; PRD §7.7 says the          ground stays still",
        city.report.buildings
    );
    assert!(
        worst * 5 < city.report.buildings * 3,
        "one add moved {worst} of {} buildings — the city was rebuilt, not grown",
        city.report.buildings
    );

    // PRD §13.1's budget is a property of the profile the product ships, and
    // this test runs in whichever profile the developer chose. The test profile
    // is about twice release because the harness itself is unoptimised. The
    // budget is asserted where it means something and reported everywhere.
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
