//! PRD §15's M1 gate.
//!
//! > **M1 — Deterministic city, static.** `polis-repo` + `polis-layout`.
//! > Generate a city from git history and render it to a window (or PNG). No
//! > agents, no live data. **Gate: byte-identical layout across two runs and
//! > across two machines.** Do not proceed until this holds.
//!
//! > Layout determinism (golden files). Fixture repos with pinned git history;
//! > snapshot the serialized `CityLayout`. Run on two OSes in CI. This is the
//! > most important test in the suite — everything else in the product depends
//! > on the map not moving. (PRD §16)
//!
//! # The six things this file asserts, and what each one can and cannot prove
//!
//! | # | Assertion | Catches |
//! |---|---|---|
//! | a | [`the_fixture_repositories_have_pinned_history`] | a fixture that is itself nondeterministic, which would make everything below meaningless |
//! | b | [`golden_hamlet`], [`golden_town`] | any change to the serialized layout, reviewably |
//! | c | [`two_runs_in_one_process_are_byte_identical`] | a mutable static, a cached value, an accumulating buffer |
//! | d | [`two_fresh_processes_agree`] | anything seeded per process — `RandomState`, ASLR-ordered pointers, an environment variable read three layers down |
//! | e | [`hash_map_iteration_order_cannot_move_the_city`], [`no_hash_map_reaches_the_layout`] | PRD §16's nondeterminism hunt |
//! | f | [`ci_runs_the_golden_test_on_ubuntu_and_windows`] | the two-OS leg silently not being wired up |
//! | g | [`a_render_written_between_two_runs_cannot_move_the_city`] | the repo-walk trap: Polis ingesting its own renders and growing the town a little every run |
//! | h | [`the_age_ramp_follows_real_commit_time`] | PRD §7.1's age ramp quietly reading list position instead of the clock, so every history draws the same city |
//!
//! # On PRD §16's "run layout twice with `HashMap` iteration randomization
//! # enabled"
//!
//! That instruction is written for a runtime with a *switch* for hash seeding —
//! Python's `PYTHONHASHSEED`, Go's map iteration. **Rust has no such switch,
//! because it needs none: `std::collections::hash_map::RandomState` seeds from
//! the OS on first use in every process, so every process already has a
//! different hash order.** The Rust-native form of PRD §16's job is therefore
//! two tests, and both are here:
//!
//! * [`two_fresh_processes_agree`] runs the layout in several fresh child
//!   processes and compares digests. Each child has its own `RandomState` seed,
//!   so if any `HashMap` iteration order reached the layout this fails — and it
//!   fails on the first run, on the developer's machine, not six months later.
//! * [`hash_map_iteration_order_cannot_move_the_city`] goes further and pushes
//!   the *input* through a `HashMap`, so the pipeline is handed its files in
//!   `RandomState` order rather than in path order.
//!
//! And because "we removed every `HashMap`" is a claim that decays the moment
//! someone adds one, [`no_hash_map_reaches_the_layout`] asserts it structurally
//! against the crate's own source rather than by inspection.
//!
//! # What this file honestly does **not** prove
//!
//! **The second machine.** Everything here runs on one host. The golden files
//! are what a second machine compares against, and the CI matrix is what makes
//! that comparison happen — but a green run here is single-machine evidence,
//! and [`ci_runs_the_golden_test_on_ubuntu_and_windows`] asserts the *workflow
//! configuration*, not that a Linux runner ever executed it. Anyone reporting
//! on the M1 gate from a local run must say so.

#![allow(clippy::float_cmp)] // determinism assertions are exact on purpose

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use polis_events::{LogicalPath, WorktreeId};
use polis_layout::city::{self, City, LayoutInputs};
use polis_repo::git::History;
use polis_repo::tree::{walk_with, WalkOptions};
use polis_repo::{FileMeta, RepoTree};

// ---------------------------------------------------------------------------
// (a) Fixtures with pinned git history
// ---------------------------------------------------------------------------

/// Commit `HEAD` of each fixture, pinned.
///
/// These are reproducible because `tests/make-fixtures.sh` pins the author, the
/// committer, both timestamps, the line endings, the file mode and the global
/// and system git config — see that script's header. Pinning them here is what
/// makes a fixture that drifted report itself *as a fixture problem*, instead of
/// showing up as an unexplained golden-file diff that looks like a layout
/// regression.
const HAMLET_HEAD: &str = "eb563a19c1c728c8ef8f613101360db728073603";

/// See [`HAMLET_HEAD`].
const TOWN_HEAD: &str = "e5fc00b4191c366d9fa67e2dd79d94acbd8ce0f3";

/// The workspace root — the directory holding `Cargo.toml` and `tests/`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("polis-layout has a parent directory")
        .to_path_buf()
}

/// A `bash` that exists on this machine.
///
/// `bash` is on `PATH` on every GitHub Actions runner, Windows included (Git for
/// Windows ships it), and the Git-for-Windows install locations are tried as a
/// fallback for a developer whose `PATH` differs. There is deliberately **no
/// skip**: a determinism gate that quietly does not run is worse than one that
/// fails loudly.
fn bash_program() -> String {
    if let Some(explicit) = std::env::var_os("POLIS_BASH") {
        return explicit.to_string_lossy().into_owned();
    }
    let candidates = [
        "bash",
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files\Git\usr\bin\bash.exe",
    ];
    for candidate in candidates {
        if Command::new(candidate)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return candidate.to_owned();
        }
    }
    panic!(
        "no `bash` found. PRD §16's fixtures are built by tests/make-fixtures.sh; \
         set POLIS_BASH to a bash executable."
    );
}

/// Builds the fixture repositories once per test binary and returns their root.
///
/// They are built under `CARGO_TARGET_TMPDIR`, which is inside `target/` and
/// therefore git-ignored: a fixture repository checked into the repository it is
/// a fixture for is a nested-repo mess, and one built into a temporary directory
/// is rebuilt from a script that can be read.
fn fixtures() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("polis-fixtures");
        let script = workspace_root().join("tests").join("make-fixtures.sh");
        assert!(script.is_file(), "missing {}", script.display());
        let output = Command::new(bash_program())
            .arg(to_bash_path(&script))
            .arg(to_bash_path(&root))
            .output()
            .expect("run tests/make-fixtures.sh");
        assert!(
            output.status.success(),
            "make-fixtures.sh failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        root
    })
}

/// A path `bash` will accept on either platform.
///
/// Git Bash understands `C:/x/y` (forward slashes) but treats `\` as an escape,
/// so a Windows path has to be re-spelled before it goes on a bash command line.
fn to_bash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// The tree the layout is generated from: the filesystem walk, with git history
/// folded onto it.
///
/// `skip_massed` is `false` for the fixtures — PRD §8 sizes the industrial mass
/// from the files inside it, and a fixture that skipped them would not exercise
/// that — and `true` for the real repository, whose `target/` directory is
/// larger than the rest of the workspace by three orders of magnitude.
fn repo_tree(root: &Path, skip_massed: bool) -> RepoTree {
    let options = WalkOptions {
        skip_massed,
        ..WalkOptions::default()
    };
    let files = walk_with(root, &options).expect("walk the checkout");
    let mut tree = RepoTree {
        root: root.to_path_buf(),
        files: BTreeMap::new(),
        worktrees: BTreeMap::new(),
        head: String::new(),
    };
    tree.worktrees
        .insert(WorktreeId::PRIMARY, root.to_path_buf());
    for meta in files {
        tree.files.insert(meta.path.clone(), meta);
    }
    History::read(root)
        .expect("read git history")
        .apply(&mut tree);
    tree
}

/// The city for a fixture, by name.
fn fixture_city(name: &str) -> City {
    city::generate_city(&repo_tree(&fixtures().join(name), false))
}

/// The city for this workspace itself, with every input the product has today.
///
/// Streets from the real import graph, monuments from the real inbound degrees,
/// heights from the real uncommitted diff. This is the city an operator would
/// see, which is what makes it worth looking at.
fn real_city() -> City {
    let root = workspace_root();
    let tree = repo_tree(&root, true);
    let imports = polis_repo::imports::ImportGraph::build(&tree);
    let inputs = LayoutInputs {
        streets: imports.cross_district_edges(),
        inbound: imports.inbound_counts(),
        // ADR-0042: the transcript is the primary source and does not exist at
        // M1, so this is the documented `polis_repo::git` fallback.
        diff_lines: polis_repo::git::diff_line_counts(&root)
            .unwrap_or_default()
            .into_iter()
            .collect(),
    };
    city::generate_with(&tree, &inputs)
}

fn lp(s: &str) -> LogicalPath {
    LogicalPath::new(s).expect("a valid logical path")
}

/// (a) The fixture itself must be deterministic, or nothing below means
/// anything.
#[test]
fn the_fixture_repositories_have_pinned_history() {
    for (name, head) in [("hamlet", HAMLET_HEAD), ("town", TOWN_HEAD)] {
        let root = fixtures().join(name);
        assert!(root.join(".git").is_dir(), "{name} was not initialised");
        let actual = polis_repo::git::head_commit(&root).expect("HEAD");
        assert_eq!(
            actual, head,
            "the {name} fixture drifted. tests/make-fixtures.sh pins the author, \
             the committer, both dates, the line endings and the file mode, so a \
             changed SHA means the SCRIPT changed — bump FIXTURE_VERSION and this \
             constant together. Until then every golden file below is comparing \
             against a different repository."
        );
    }

    let town = repo_tree(&fixtures().join("town"), false);

    // Nested four deep.
    assert!(town
        .files
        .contains_key(&lp("src/auth/providers/oauth/google.rs")));

    // Non-ASCII, in NFC (ADR-0028: no Unicode normalisation, ASCII case folding
    // only; ADR-0046: byte slicing, so these are a panic risk, not a wrong
    // answer).
    for name in ["src/café/módulo.rs", "src/café/naïve.rs", "docs/日本語.md"] {
        assert!(
            town.files.contains_key(&lp(name)),
            "{name} did not survive the walk"
        );
    }

    // The deleted file: gone from the working tree, still in history. PRD §7.5
    // is what turns that into a vacant lot rather than a hole.
    assert!(!town.files.contains_key(&lp("src/legacy/old_client.rs")));
    let growth = polis_repo::git::GrowthSequence::bootstrap(&fixtures().join("town"))
        .expect("growth sequence");
    assert!(
        growth.index_of(&lp("src/legacy/old_client.rs")).is_some(),
        "git forgot a file it once added"
    );

    // The renamed file. `polis-repo` pins `--no-renames`, so this is a delete
    // plus an add: the old path is in history and not on disk, the new path is
    // in both.
    assert!(!town.files.contains_key(&lp("src/util.rs")));
    assert!(town.files.contains_key(&lp("src/utils/helpers.rs")));
    assert!(growth.index_of(&lp("src/util.rs")).is_some());
    assert!(growth.index_of(&lp("src/utils/helpers.rs")).is_some());

    // The industrial tree PRD §8 draws as one dull mass.
    assert!(town
        .files
        .values()
        .any(|m| m.class == polis_repo::FileClass::Industrial));

    // Growth order is history, not path order (PRD §7.1): the founding files
    // must come before the last district.
    let founding = town.files[&lp("src/main.rs")].growth_index;
    let periphery = town.files[&lp("src/ui/app.tsx")].growth_index;
    assert!(
        founding < periphery,
        "growth order is not commit order: {founding} vs {periphery}"
    );

    // Sizes differ, so PRD §7.3's sqrt(bytes) footprint has something to say.
    let sizes: Vec<u64> = town.files.values().map(|m| m.size_bytes).collect();
    let min = sizes.iter().copied().min().expect("files");
    let max = sizes.iter().copied().max().expect("files");
    assert!(max > min * 20, "the fixture's files are all the same size");
}

// ---------------------------------------------------------------------------
// (b) Golden files
// ---------------------------------------------------------------------------

/// Snapshots live in `tests/golden/` at the workspace root rather than beside
/// this file, because they are the artifact PRD §16 is about and reviewing a
/// change to them is reviewing a change to the product.
macro_rules! golden {
    ($name:expr, $value:expr) => {
        insta::with_settings!({snapshot_path => "../../tests/golden", prepend_module_to_snapshot => false}, {
            insta::assert_snapshot!($name, $value);
        });
    };
}

/// (b) The smallest city there is, so a golden-file diff is readable.
#[test]
fn golden_hamlet() {
    let city = fixture_city("hamlet");
    golden!("hamlet", city.snapshot().expect("serializes"));
}

/// (b) The one that exercises nesting, non-ASCII, a deletion, a rename and an
/// industrial tree.
#[test]
fn golden_town() {
    let city = fixture_city("town");
    golden!("town", city.snapshot().expect("serializes"));
}

/// (b) The town with agents working on it: PRD §9's streets, PRD §8's
/// import-derived monuments, and PRD §7.3's heights.
///
/// Without this the golden files would pin a city whose `streets` array is
/// always empty and whose buildings are all at base height — three whole
/// features that could break with no test noticing.
///
/// The diff-line table is written down rather than measured, deliberately: PRD
/// §7.3's heights come from the *transcript* (ADR-0042), which does not exist at
/// M1, and re-diffing a clean fixture would give zeros. Every number here is a
/// plausible agent's working state.
#[test]
fn golden_town_in_use() {
    let tree = repo_tree(&fixtures().join("town"), false);
    let imports = polis_repo::imports::ImportGraph::build(&tree);
    let inputs = LayoutInputs {
        streets: imports.cross_district_edges(),
        inbound: imports.inbound_counts(),
        diff_lines: [
            ("src/auth/session.rs", 420_u32),
            ("src/auth/providers/oauth/google.rs", 65),
            ("src/net/server.rs", 12),
            ("src/ui/app.tsx", 1_900),
            ("src/café/módulo.rs", 3),
            ("docs/日本語.md", 140),
        ]
        .into_iter()
        .map(|(path, lines)| (lp(path), lines))
        .collect(),
    };
    let city = city::generate_with(&tree, &inputs);

    // The tallest thing on the map is the biggest unreviewed pile (PRD §7.3).
    let tallest = city
        .layout
        .buildings
        .values()
        .max_by(|a, b| a.height.total_cmp(&b.height))
        .expect("buildings");
    assert_eq!(
        tallest.path.as_str(),
        "src/ui/app.tsx",
        "the 1900-line pile is not the tallest building"
    );

    golden!("town-in-use", city.snapshot().expect("serializes"));
}

/// (b) The golden files must actually **contain** a street.
///
/// This is the assertion that would have caught the release-only `build_streets`
/// bug at review time rather than at the gate. That bug was a `Vec` returned in
/// caller order instead of canonical order, and it survived 57 determinism tests
/// because every fixture's import graph was empty: all three goldens pinned
/// `streets: []`, and a field that is always empty cannot regress *visibly* in a
/// snapshot diff.
///
/// A coverage claim decays the moment someone edits a fixture, so it is asserted
/// against the pinned snapshot itself rather than stated in a comment. If this
/// fails, `tests/make-fixtures.sh` stopped producing a cross-district import and
/// PRD §9's streets are once again uncovered by every golden file.
#[test]
fn the_goldens_pin_a_non_empty_street_array() {
    let snap = workspace_root()
        .join("tests")
        .join("golden")
        .join("town-in-use.snap");
    let text =
        std::fs::read_to_string(&snap).unwrap_or_else(|e| panic!("read {}: {e}", snap.display()));
    assert!(
        !text.contains("\"streets\": []"),
        "{} pins an empty `streets` array, so no golden file covers PRD §9's          streets and the field can be reordered without any snapshot noticing",
        snap.display()
    );
    // And more than one, or their *order* is still uncovered.
    let streets = text
        .split("\"streets\":")
        .nth(1)
        .expect("the snapshot has a streets field");
    assert!(
        streets.matches("\"from\":").count() > 1,
        "{} pins fewer than two streets, so their canonical order is not          covered by any golden file",
        snap.display()
    );
}

/// A city has to actually be a city before its golden file is worth anything.
/// A pipeline that produced no roads would snapshot cleanly forever.
#[test]
fn the_golden_cities_are_cities() {
    let town = fixture_city("town");
    let report = town.report;
    assert!(report.files > 20, "{report:?}");
    assert!(report.buildings > 15, "{report:?}");
    assert!(report.blocks > 5, "{report:?}");
    assert!(
        report.cycles > 0,
        "the road graph is a tree. PRD §7.2: without snapping you get a tree, \
         and trees read as artificial. {report:?}"
    );
    assert!(report.junctions > 0, "{report:?}");
    assert!(report.monuments > 0, "PRD §8 found no anchors: {report:?}");
    assert!(report.massed > 0, "PRD §8 massed nothing: {report:?}");
    assert!(!town.industrial.is_empty());
    assert!(town.layout.districts.len() > 4, "{report:?}");
}

// ---------------------------------------------------------------------------
// (c) Two runs in one process
// ---------------------------------------------------------------------------

/// PRD §7.1's age gradient for a city: median block area on the newest ground
/// over the oldest.
///
/// `CityReport` carries it as an integer so the struct stays `Eq` and a golden
/// file cannot drift by one bit of an `f64` (see its documentation); this is the
/// same number as a ratio.
fn age_gradient(city: &City) -> f64 {
    f64::from(city.report.age_gradient_x100) / 100.0
}

/// (g) **The repo-walk trap**, end to end.
///
/// `polis-repo`'s own tests prove the walk ignores an excluded path. This proves
/// the *city* does — the property the operator actually has — and it proves it
/// on a real checkout with real git history rather than on a synthesised file
/// list.
///
/// The bug it guards against is not hypothetical. The city-layout design
/// bake-off rendered its candidates into `docs/design/`, which is inside this
/// repository, so each run laid out a repository that the previous run had made
/// larger. The city drifted, nothing errored, and PRD §7.4's whole promise was
/// quietly false.
///
/// Written as generate → write → generate in one test, in that order, because a
/// feedback loop between consecutive runs is invisible to two independent ones.
#[test]
// The guard type is declared where it is used and nowhere else, which is the
// point of it.
#[allow(clippy::items_after_statements)]
fn a_render_written_between_two_runs_cannot_move_the_city() {
    let root = fixtures().join("town");
    let probe = root.join("docs").join("design").join("city-probe.png");
    // A guard, so a failed assertion cannot leave the fixture dirty for the
    // rest of the binary: the fixture is shared and built once.
    struct Probe(PathBuf);
    impl Drop for Probe {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_dir(self.0.parent().expect("parent"));
        }
    }

    let before = city::generate_city(&repo_tree(&root, false));
    std::fs::create_dir_all(probe.parent().expect("parent")).expect("mkdir docs/design");
    let _guard = Probe(probe.clone());
    std::fs::write(
        &probe,
        [0x89u8, b'P', b'N', b'G', 13, 10, 26, 10, 7, 7, 7, 7],
    )
    .expect("write the probe render");
    let after = city::generate_city(&repo_tree(&root, false));

    assert_eq!(
        before.digest(),
        after.digest(),
        "a render written into docs/design/ between two runs moved the city; \
         `polis_repo::tree::WalkExclusions` is what stops that"
    );
    assert_eq!(before.report.files, after.report.files);
    assert!(
        !after
            .layout
            .buildings
            .keys()
            .any(|p| p.as_str().starts_with("docs/design")),
        "an excluded path got a building"
    );
    // And the exclusion is a *rule*, not a blanket ban on `docs/`: the town
    // fixture's own documents are still districts.
    assert!(
        after
            .layout
            .buildings
            .keys()
            .any(|p| p.as_str() == "docs/guide.md"),
        "the exclusion took the whole docs/ district with it"
    );
}

/// (h) **PRD §7.1's age ramp reads clocks, not ranks**, at 5 000 files.
///
/// The four histories the ramp has to degrade honestly through, each built by
/// rewriting `added_at` on one synthetic corpus so that *nothing else about the
/// repository changes* — same paths, same sizes, same growth order. Anything
/// that moves is the ramp.
///
/// This is the test that would have failed before the ramp was calibrated on
/// commit time: with the ramp on growth-sequence position, all four histories
/// produce the identical city, because none of them changes a file's position.
#[test]
// The casts are file indices and day counts, all far below 2^53; `housed` and
// `shared` are two different questions about the same file and the names are the
// clearest ones there are.
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::similar_names
)]
fn the_age_ramp_follows_real_commit_time() {
    use polis_layout::age::AgeRampKind;

    const DAY_MS: i64 = 86_400_000;
    const BASE_MS: i64 = 1_500_000_000_000;

    /// One corpus, with every commit time rewritten by `at`.
    fn with_history(at: &dyn Fn(usize, usize) -> i64) -> City {
        let mut tree = polis_repo::synthetic::repository(5_000, GATE_SEED);
        let n = tree.files.len();
        let mut ordered: Vec<polis_events::LogicalPath> = tree.files.keys().cloned().collect();
        ordered.sort_by_key(|p| tree.files[p].growth_index);
        for (i, path) in ordered.iter().enumerate() {
            let meta = tree.files.get_mut(path).expect("present");
            meta.added_at = polis_events::WallTime::from_unix_millis(at(i, n));
            meta.last_touched = meta.added_at;
        }
        city::generate_city(&tree)
    }

    // (1) One commit. There is no age information, so none is drawn.
    let squashed = with_history(&|_, _| BASE_MS);
    assert_eq!(squashed.report.age_ramp, AgeRampKind::Uniform);
    assert_eq!(squashed.report.history_days, 0);
    assert!(
        (age_gradient(&squashed) - 1.0).abs() < 0.35,
        "a squash-imported repository was given an age gradient of {:.2}x it does not have",
        age_gradient(&squashed)
    );

    // (2) Three weeks old. Real ordering, no absolute calibration possible.
    let young = with_history(&|i, n| BASE_MS + (i as i64) * 21 * DAY_MS / (n as i64).max(1));
    assert_eq!(young.report.age_ramp, AgeRampKind::Relative);
    assert_eq!(young.report.history_days, 20);
    assert_eq!(
        young.report.old_town_files, young.report.files,
        "every file of a three-week-old repository is inside its first year"
    );

    // (3) A decade, with a wholesale import at the root: a third of the files
    //     land in the founding commit, the rest over the next twelve years.
    //     This is Neovim's shape, and it is the one that should have the
    //     largest, finest old town.
    let imported = with_history(&|i, n| {
        let founding = n / 3;
        if i < founding {
            BASE_MS
        } else {
            let k = (i - founding) as i64;
            let rest = (n - founding).max(1) as i64;
            BASE_MS + 400 * DAY_MS + k * 12 * 365 * DAY_MS / rest
        }
    });
    assert_eq!(imported.report.age_ramp, AgeRampKind::Calibrated);
    assert!(
        imported.report.old_town_files >= imported.report.files / 3,
        "{} of {} files were added in year one and only {} were counted",
        imported.report.files / 3,
        imported.report.files,
        imported.report.old_town_files
    );

    // (4) Twenty years, and only a handful of files survive from year one —
    //     Django's shape, and the case PRD §7.1 was silently failing. Those few
    //     files are *still* the old town.
    let rewritten = with_history(&|i, n| {
        let early = n / 40;
        if i < early {
            BASE_MS + (i as i64) * DAY_MS
        } else {
            let k = (i - early) as i64;
            let rest = (n - early).max(1) as i64;
            BASE_MS + 800 * DAY_MS + k * 19 * 365 * DAY_MS / rest
        }
    });
    assert_eq!(rewritten.report.age_ramp, AgeRampKind::Calibrated);
    assert!(
        rewritten.report.old_town_files <= rewritten.report.files / 20,
        "{} first-year files is not the 2.5% this history has",
        rewritten.report.old_town_files
    );

    // The point of the whole exercise: four different histories over one
    // unchanged file list must be four different cities.
    let digests = [
        squashed.digest(),
        young.digest(),
        imported.digest(),
        rewritten.digest(),
    ];
    for i in 0..digests.len() {
        for j in i + 1..digests.len() {
            assert_ne!(
                digests[i], digests[j],
                "histories {i} and {j} produced the same city; the ramp is not \
                 reading commit time"
            );
        }
    }

    // And the one that has a real founding cohort has the coarsest rim relative
    // to its core — PRD §7.1's age structure, measured.
    println!(
        "POLIS_AGE_RAMP squashed={:.2}x young={:.2}x imported={:.2}x rewritten={:.2}x",
        age_gradient(&squashed),
        age_gradient(&young),
        age_gradient(&imported),
        age_gradient(&rewritten),
    );
    assert!(
        age_gradient(&imported) > age_gradient(&squashed),
        "the imported repository ({:.2}x) has no more age structure than the \
         squashed one ({:.2}x)",
        age_gradient(&imported),
        age_gradient(&squashed)
    );
}

/// (c) Catches a mutable static, a memoised value, a buffer that accumulates
/// across calls — everything a golden file compared once cannot see.
#[test]
fn two_runs_in_one_process_are_byte_identical() {
    for name in ["hamlet", "town"] {
        let first = fixture_city(name).snapshot().expect("serializes");
        for run in 1..5 {
            let again = fixture_city(name).snapshot().expect("serializes");
            assert!(
                first == again,
                "the {name} city moved between run 0 and run {run} of one process"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// (d) Two fresh processes
// ---------------------------------------------------------------------------

/// (d) The one an in-process test cannot do.
///
/// A fresh process has a fresh address space and a fresh
/// `RandomState`. Anything that leaked either into the layout differs here and
/// is invisible above.
#[test]
fn two_fresh_processes_agree() {
    let mine = child_expected();
    let mut seen = Vec::new();
    for _ in 0..3 {
        seen.push(child_digests());
    }
    for (i, child) in seen.iter().enumerate() {
        assert_eq!(
            *child, mine,
            "child process {i} produced a different city. A fresh process has a \
             fresh `RandomState` and a fresh address space, so this is what \
             PRD §16's nondeterminism hunt looks like on Rust."
        );
    }
}

/// The digests this process computes, in the order the child prints them.
fn child_expected() -> Vec<(String, u64)> {
    ["hamlet", "town"]
        .into_iter()
        .map(|name| (name.to_owned(), fixture_city(name).digest()))
        .collect()
}

/// Re-invokes this test binary and reads back the digests it prints.
fn child_digests() -> Vec<(String, u64)> {
    let exe = std::env::current_exe().expect("test binary path");
    let output = Command::new(exe)
        .args([
            "--exact",
            "print_fixture_digests",
            "--ignored",
            "--nocapture",
        ])
        // Pinned, not inherited: see `child_markers`.
        .env("RUST_TEST_THREADS", "1")
        .output()
        .expect("re-invoke the test binary");
    assert!(
        output.status.success(),
        "child process failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Vec<(String, u64)> = child_markers(&stdout, "POLIS_CITY_DIGEST ")
        .into_iter()
        .filter_map(|token| {
            let (name, digest) = token.split_once('=')?;
            Some((name.to_owned(), u64::from_str_radix(digest, 16).ok()?))
        })
        .collect();
    assert_eq!(parsed.len(), 2, "child printed no digests:\n{stdout}");
    parsed
}

/// Every whitespace-delimited token that follows `marker` in a child's stdout.
///
/// **Position-independent on purpose, and this is not defensive style — a
/// line-anchored parser here is a live bug.** The child inherits this process's
/// environment, `RUST_TEST_THREADS` included, and in single-threaded mode
/// libtest writes `test <name> ... ` with *no trailing newline* before running
/// the test. The child's first `--nocapture` line therefore arrives as
/// `test print_fixture_digests ... POLIS_CITY_DIGEST hamlet=…`, and a parser
/// anchored to the start of the line finds nothing. The test then panics on its
/// own output format instead of comparing two processes, so the assertion that
/// catches `RandomState` reaching the layout silently stops running — in
/// precisely the CI job that sets `RUST_TEST_THREADS: 1` in order to run it.
fn child_markers<'a>(stdout: &'a str, marker: &str) -> Vec<&'a str> {
    stdout
        .split(marker)
        .skip(1)
        .filter_map(|rest| rest.split_whitespace().next())
        .collect()
}

/// The child half of [`two_fresh_processes_agree`]. Ignored, so it only ever
/// runs when that test asks for it by name.
#[test]
#[ignore = "child process of two_fresh_processes_agree"]
fn print_fixture_digests() {
    for name in ["hamlet", "town"] {
        println!(
            "POLIS_CITY_DIGEST {name}={:016x}",
            fixture_city(name).digest()
        );
    }
}

// ---------------------------------------------------------------------------
// (e) PRD §16's nondeterminism hunt
// ---------------------------------------------------------------------------

/// (e) The input arrives in `RandomState` order and the city does not move.
///
/// PRD §7.4 names this failure first: "never from iteration order of a
/// `HashMap`". A `HashMap` with more than a handful of keys iterates in an
/// order that is randomised per process, so building the tree out of one is the
/// cheapest available adversary — and it exercises the paths a caller would
/// take if `polis-repo` ever handed over an unordered collection.
#[test]
fn hash_map_iteration_order_cannot_move_the_city() {
    for name in ["hamlet", "town"] {
        let ordered = repo_tree(&fixtures().join(name), false);
        let scrambled: HashMap<LogicalPath, FileMeta> = ordered
            .files
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut shuffled = RepoTree {
            root: ordered.root.clone(),
            files: BTreeMap::new(),
            worktrees: ordered.worktrees.clone(),
            head: ordered.head.clone(),
        };
        for (path, meta) in scrambled {
            shuffled.files.insert(path, meta);
        }
        assert!(
            city::generate_city(&ordered)
                .snapshot()
                .expect("serializes")
                == city::generate_city(&shuffled)
                    .snapshot()
                    .expect("serializes"),
            "the {name} city depends on the order its files were handed over"
        );
    }
}

/// (e) The same adversary aimed at [`LayoutInputs`], which is the *other* way
/// unordered data reaches the layout — and the one that got through.
///
/// `RepoTree::files` is a `BTreeMap`, so the tree can only ever arrive sorted.
/// `LayoutInputs::streets` is a plain `Vec` supplied by the caller, so it can
/// arrive in any order at all, and `build_streets` used to *preserve* that
/// order: it asserted the input was canonical and returned it unsorted. Because
/// `debug_assert_canonical_order` compiles to nothing when `debug_assertions` is
/// off, the assertion held in `cargo test` and the layout moved in `--release`,
/// which is the profile the product ships. Reversing `streets` changed the
/// snapshot SHA-256 in release and not in debug.
///
/// `build_streets` now sorts unconditionally, so this holds in both profiles.
///
/// **The permutation half only runs in release**, because in a debug build the
/// input assertion fires first — correctly, since an unsorted `streets` really
/// is an upstream bug worth a panic. The output check below runs in both, and
/// CI's nondeterminism job runs this file in release for exactly this reason.
///
/// This runs against **the workspace itself, not a fixture**, and that is not a
/// convenience: the workspace has an order of magnitude more import edges than
/// the fixture, so it is the harder permutation. The fixture used to have *no*
/// cross-district edge at all, which is precisely why this bug survived — a
/// golden file that never contained a street cannot notice the streets being
/// reordered. `town` now carries three (see `tests/make-fixtures.sh` commit 9)
/// and [`the_goldens_pin_a_non_empty_street_array`] refuses to let that go back
/// to zero. The assertion below still refuses to pass vacuously.
#[test]
fn caller_supplied_input_order_cannot_move_the_city() {
    let root = workspace_root();
    let tree = repo_tree(&root, true);
    let imports = polis_repo::imports::ImportGraph::build(&tree);
    let ordered = LayoutInputs {
        streets: imports.cross_district_edges(),
        inbound: imports.inbound_counts(),
        diff_lines: BTreeMap::new(),
    };
    assert!(
        ordered.streets.len() > 1,
        "this workspace produced {} cross-district import edges; the test needs \
         at least two to permute, so it would otherwise pass vacuously. Both \
         golden fixtures already have an empty `streets` array — if this input \
         goes empty too, nothing anywhere covers PRD §9's streets.",
        ordered.streets.len()
    );
    let reference = city::generate_with(&tree, &ordered);

    // Holds in every profile: the serialized order is the layout's own, not the
    // caller's, so a golden file cannot be permuted by an upstream change.
    let emitted: Vec<(LogicalPath, LogicalPath)> = reference
        .layout
        .streets
        .iter()
        .map(|s| (s.from.clone(), s.to.clone()))
        .collect();
    let mut sorted = emitted.clone();
    sorted.sort();
    assert_eq!(
        emitted, sorted,
        "`CityLayout::streets` is not in canonical order, so it is carrying \
         whatever order the caller supplied into the snapshot (PRD §7.4)"
    );

    if cfg!(debug_assertions) {
        return;
    }
    let expected = reference.snapshot().expect("serializes");
    for permuted in [
        {
            let mut i = ordered.clone();
            i.streets.reverse();
            i
        },
        {
            let mut i = ordered.clone();
            i.streets.rotate_left(1);
            i
        },
        {
            let mut i = ordered.clone();
            i.streets.reverse();
            i.inbound.reverse();
            i
        },
    ] {
        assert!(
            city::generate_with(&tree, &permuted)
                .snapshot()
                .expect("serializes")
                == expected,
            "the city moved when `LayoutInputs` was permuted. This is the \
             release-only hole: a `debug_assert` cannot enforce PRD §7.4, \
             because the artifact must be identical in the profile that ships."
        );
    }
}

/// (e) The structural half: assert there is no `HashMap` in the layout crate,
/// rather than claiming it by inspection.
///
/// PRD §16 asks for a job that "runs layout twice with `HashMap` iteration
/// randomization enabled". Rust has no such switch — `RandomState` is always
/// per-process — so the executable half of that instruction is
/// [`two_fresh_processes_agree`], and this is the half that stops a `HashMap`
/// being *added* between now and the next time anyone looks.
///
/// The scan covers non-test code only: each file is truncated at its
/// `#[cfg(test)]`, because [`hash_map_iteration_order_cannot_move_the_city`] and
/// its in-crate equivalent are *supposed* to use a `HashMap` — that is the
/// adversary. Full-line comments are stripped, so the module documentation in
/// `determinism.rs`, which discusses `HashMap` at length, does not trip it.
///
/// `mul_add` is on the list for the same reason and is handled differently: it
/// is genuinely present in `lib.rs`'s geometry, it is a *recorded* disagreement
/// with `determinism.rs` rule 5 rather than an oversight, and the right
/// treatment for a known exception is an exact count rather than an allow-list
/// that would also hide the sixth one.
/// Tokens that cannot occur in prose, and why each one is banned.
///
/// `HashMap` in a doc comment or a panic message is a warning to the reader;
/// `HashMap<` is a use.
const BANNED_IN_LAYOUT: &[(&str, &str)] = &[
    (
        "HashMap<",
        "iteration order is randomised per process (PRD §7.4)",
    ),
    (
        "HashMap::",
        "iteration order is randomised per process (PRD §7.4)",
    ),
    (
        "HashSet<",
        "iteration order is randomised per process (PRD §7.4)",
    ),
    (
        "HashSet::",
        "iteration order is randomised per process (PRD §7.4)",
    ),
    ("RandomState", "seeded from the OS in every process"),
    (
        "DefaultHasher",
        "SipHash's output is not stable across Rust releases (ADR-0029)",
    ),
    ("ahash", "AHashMap's RandomState is seeded per process"),
    (
        "read_dir",
        "filesystem order differs between NTFS, ext4 and APFS",
    ),
    (
        "SystemTime",
        "the wall clock may not reach the layout (PRD §7.4)",
    ),
    (
        "Instant::now",
        "the clock may not reach the layout (PRD §7.4)",
    ),
    (
        "rand::",
        "a rand generator may change its stream in a patch release",
    ),
    (
        "mul_add",
        "a fused multiply-add rounds once and the unfused pair twice",
    ),
];

/// `Vec2::{length_squared, dot, cross}`, `Polygon::{signed_area_x2, centroid}`.
const MUL_ADD_SITES_IN_LIB: usize = 5;

/// Every non-test line of `polis-layout/src` that names a banned token.
fn scan_layout_sources() -> Vec<String> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&src)
        .expect("read polis-layout/src")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .collect();
    files.sort();
    assert!(files.len() >= 8, "found only {} modules", files.len());

    let mut findings = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read a module");
        let markers = text.matches("#[cfg(test)]").count();
        assert_eq!(
            markers,
            1,
            "{} has {markers} `#[cfg(test)]` markers; this scan truncates at the \
             first one and would silently stop covering the code after a second.",
            file.display(),
        );
        let code = text.split("#[cfg(test)]").next().unwrap_or_default();
        for (number, line) in code.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for (token, why) in BANNED_IN_LAYOUT {
                if line.contains(token) {
                    findings.push(format!(
                        "{}:{}: `{token}` — {why}\n    {}",
                        file.display(),
                        number + 1,
                        line.trim()
                    ));
                }
            }
        }
    }
    findings
}

#[test]
fn no_hash_map_reaches_the_layout() {
    let findings = scan_layout_sources();

    // `lib.rs`'s geometry uses `f32::mul_add`, which `determinism.rs` rule 5
    // forbids. It is a known, recorded disagreement rather than an oversight
    // (Rust guarantees a single rounding for an explicit `mul_add`, and
    // IEEE-754 requires `fma` to be correctly rounded, so it is deterministic on
    // any conforming target) — but it is exactly the kind of thing that should
    // be visible in a determinism gate rather than buried in a handover note.
    let (known, unexpected): (Vec<_>, Vec<_>) = findings
        .into_iter()
        .partition(|f| f.contains("mul_add") && f.contains("lib.rs"));
    assert!(
        unexpected.is_empty(),
        "nondeterminism reached the layout crate:\n{}",
        unexpected.join("\n")
    );
    assert_eq!(
        known.len(),
        MUL_ADD_SITES_IN_LIB,
        "the `mul_add` count in lib.rs changed. There are exactly \
         {MUL_ADD_SITES_IN_LIB} today — `Vec2::length_squared`, `Vec2::dot`, \
         `Vec2::cross`, `Polygon::signed_area_x2` and `Polygon::centroid` — and \
         every one of them reaches the layout, through `snap_target`'s distance \
         test and every area comparison in `blocks` and `lots`. A new one is a \
         decision, not a refactor:\n{}",
        known.join("\n")
    );
}

// ---------------------------------------------------------------------------
// (f) The CI matrix
// ---------------------------------------------------------------------------

/// (f) PRD §16 demands two operating systems. This asserts the workflow is
/// wired up for them.
///
/// **It cannot assert that a Linux runner ever executed.** A green run of this
/// test on Windows is evidence about the *configuration*, and anyone reporting
/// on the M1 gate from a local run has to say which half they have.
#[test]
fn ci_runs_the_golden_test_on_ubuntu_and_windows() {
    let workflow = workspace_root()
        .join(".github")
        .join("workflows")
        .join("ci.yml");
    let text = std::fs::read_to_string(&workflow)
        .unwrap_or_else(|e| panic!("read {}: {e}", workflow.display()));

    for os in ["ubuntu-latest", "windows-latest"] {
        assert!(
            text.contains(os),
            "{} has no {os} leg. PRD §16: run on two OSes in CI.",
            workflow.display()
        );
    }
    assert!(
        text.contains("cargo test --workspace"),
        "the two-OS job does not run the test suite, so it does not run the \
         golden-file test"
    );
    assert!(
        text.contains("os: [ubuntu-latest, windows-latest]"),
        "the two OSes are named somewhere in the file but not as one job's \
         matrix, so they may not both run the same job"
    );
    // The job that runs the matrix must be the one that runs the tests: a
    // matrix on a job that only builds proves nothing about the golden file.
    let matrix_at = text
        .find("os: [ubuntu-latest, windows-latest]")
        .expect("checked above");
    let tests_at = text.find("cargo test --workspace").expect("checked above");
    assert!(
        matrix_at < tests_at,
        "`cargo test --workspace` runs before the OS matrix is declared, so it \
         is probably in a different job"
    );
    let between = &text[matrix_at..tests_at];
    assert!(
        !between.contains("\n  nondeterminism:") && !between.contains("\n  docs:"),
        "another job starts between the OS matrix and `cargo test --workspace`, \
         so the golden test is not the one running on two operating systems"
    );

    // And everything above covers **debug only**. The bug that took M1 red the
    // first time was release-only, so the release leg has to run on both
    // operating systems as well — otherwise the two-OS matrix covers debug, the
    // release job covers one OS, and release-on-Windows is covered by neither.
    let job = text
        .find("\n  nondeterminism:")
        .expect("no nondeterminism job in the workflow");
    let rest = &text[job..];
    let end = rest[1..].find("\n  docs:").map_or(rest.len(), |i| i + 1);
    let job_text = &rest[..end];
    assert!(
        job_text.contains("cargo test --release"),
        "the nondeterminism job does not run the suite in release. A \
         `debug_assert` is not an invariant, and the profile that ships is the \
         one that has to be byte-identical."
    );
    assert!(
        job_text.contains("os: [ubuntu-latest, windows-latest]"),
        "the release determinism suite runs on one operating system only. PRD \
         §15's gate is byte-identical across two MACHINES, and a release-only \
         bug would have been platform-only just as easily."
    );
}

// ---------------------------------------------------------------------------
// The real repository
// ---------------------------------------------------------------------------

/// PRD §15's gate, run against this workspace rather than a fixture.
///
/// The fixtures are the *reviewable* half — small, pinned, diffable. This is the
/// half that says the pipeline survives a real repository: real directory
/// depths, real size distribution, untracked files, a `target/` tree, and
/// whatever eight agents have left in the working tree.
///
/// Prints the report, because "does the gate hold" is a question someone asks
/// out loud and the numbers are the answer.
#[test]
fn the_real_repository_holds_the_gate() {
    let root = workspace_root();
    let tree = repo_tree(&root, true);
    assert!(
        tree.files.len() > 30,
        "only {} files found under {}",
        tree.files.len(),
        root.display()
    );

    let first = city::generate_city(&tree);
    let second = city::generate_city(&tree);
    assert!(
        first.snapshot().expect("serializes") == second.snapshot().expect("serializes"),
        "the real repository's city moved between two runs in one process"
    );
    assert_eq!(first.digest(), second.digest());

    let report = first.report;
    println!("POLIS_REAL_CITY digest={:016x}", first.digest());
    println!("POLIS_REAL_CITY report={report:?}");
    println!(
        "POLIS_REAL_CITY districts={} monuments={} industrial={} streets={}",
        first.layout.districts.len(),
        first.monuments.len(),
        first.industrial.len(),
        first.layout.streets.len()
    );

    assert!(report.blocks > 0, "no block: {report:?}");
    assert!(
        report.cycles > 0,
        "the real repository grew a tree, not a city: {report:?}"
    );
    assert!(report.buildings > 20, "{report:?}");
    assert!(report.monuments > 0, "{report:?}");
}

/// Writes the real repository's snapshot where `tests/render-city.py` can find
/// it, so the layout can be looked at rather than only asserted about.
///
/// > A determinism test proves the map does not move; it does not prove the map
/// > looks like a city.
///
/// Ignored, because it writes into `target/`:
///
/// ```text
/// cargo test -p polis-layout --test m1_gate -- --ignored --exact dump_the_real_city
/// python tests/render-city.py target/city-m1.json docs/city-m1.png
/// ```
/// Writes a synthetic thousand-file city, for the same reason.
///
/// This workspace has 86 files in a dozen districts. A layout that looks sparse
/// there could be sparse because the algorithm is wrong or because the
/// repository is small, and those need completely different fixes. Rendering a
/// repository the size PRD §13.1 budgets for — 5 000 files — is how the two are
/// told apart, and it is also the only place the cold-start budget is visible.
///
/// ```text
/// cargo test -p polis-layout --test m1_gate -- --ignored --exact dump_a_large_city
/// python tests/render-city.py target/city-large.json docs/city-large.png
/// ```
#[test]
#[ignore = "writes target/city-large.json for tests/render-city.py"]
fn dump_a_large_city() {
    let mut tree = RepoTree {
        root: PathBuf::from("/synthetic"),
        files: BTreeMap::new(),
        worktrees: BTreeMap::new(),
        head: "synthetic".to_owned(),
    };
    let mut growth = 0_u32;
    for crate_index in 0..12_u64 {
        for module in 0..8_u64 {
            for file in 0..16_u64 {
                let path = lp(&format!(
                    "crate{crate_index:02}/src/mod{module:02}/file{file:02}.rs"
                ));
                let mut meta = FileMeta::untracked(
                    path.clone(),
                    600 + (crate_index * 811 + file * 137) % 30_000,
                );
                meta.growth_index = growth;
                growth += 1;
                tree.files.insert(path, meta);
            }
        }
    }
    let started = std::time::Instant::now();
    let city = city::generate_city(&tree);
    let elapsed = started.elapsed();

    let out = workspace_root().join("target").join("city-large.json");
    std::fs::create_dir_all(out.parent().expect("target/")).expect("create target/");
    std::fs::write(&out, city.snapshot().expect("serializes")).expect("write the snapshot");
    println!("wrote {} in {elapsed:?}", out.display());
    println!("report {:?}", city.report);
}

#[test]
#[ignore = "writes target/city-m1.json for tests/render-city.py"]
fn dump_the_real_city() {
    let city = real_city();
    let out = workspace_root().join("target").join("city-m1.json");
    std::fs::create_dir_all(out.parent().expect("target/")).expect("create target/");
    std::fs::write(&out, city.snapshot().expect("serializes")).expect("write the snapshot");
    println!("wrote {}", out.display());
    println!("report {:?}", city.report);
}

// ---------------------------------------------------------------------------
// (g) The accretion architecture's own invariants
//
// Added with the port of the winning design from the bake-off
// (`docs/design/accretion/DESIGN.md`). The road network is now the boundary
// network of the settled ground rather than a space-colonisation tree, and these
// are the properties that construction is supposed to guarantee. Each one is a
// property the previous pipeline silently failed.
// ---------------------------------------------------------------------------

/// The free tripwire: `faces == E − V + C` on a fixed corpus.
///
/// > The face walk is the one piece where a subtle bug is invisible until the
/// > block count is wrong, so it wants a golden test asserting
/// > `faces == E − V + C` on a fixed corpus — that identity held at both scales
/// > here and is a cheap invariant. (`docs/design/accretion/DESIGN.md` §9)
///
/// It is an *identity*, not a threshold: every bounded face of a connected
/// planar graph is one independent cycle. A face walk that dropped a face, or
/// counted the unbounded one, or walked one twice, breaks it immediately — and
/// nothing downstream would notice, because a city with one block too few still
/// serializes cleanly.
#[test]
fn eulers_formula_holds_on_a_fixed_corpus() {
    let mut checked = 0;
    for name in ["hamlet", "town"] {
        let report = fixture_city(name).report;
        assert_eq!(
            report.blocks,
            (report.road_segments + report.components).saturating_sub(report.road_nodes),
            "{name}: {} faces against E-V+C = {}",
            report.blocks,
            report.road_segments + report.components - report.road_nodes
        );
        assert!(report.blocks > 0, "{name} has no blocks at all");
        checked += 1;
    }
    for files in [200_usize, 1_000, 3_000] {
        let city = city::generate_city(&polis_repo::synthetic::repository(files, GATE_SEED));
        let report = city.report;
        assert_eq!(
            report.blocks,
            (report.road_segments + report.components).saturating_sub(report.road_nodes),
            "synthetic {files}: {report:?}"
        );
        assert_eq!(report.cycles, report.blocks);
        checked += 1;
    }
    assert_eq!(checked, 5, "the corpus shrank");
}

/// The seed every synthetic corpus in this file uses. Fixed, so the corpus is a
/// corpus rather than a sample.
const GATE_SEED: u64 = 0xACCE_7107_0000_0001;

/// The structural properties the design is supposed to guarantee, at the scale
/// PRD §13.1 budgets for.
///
/// The numbers on the right of each assertion are the design bake-off's measured
/// baseline at 5 000 files, so a regression reads as a regression rather than as
/// an unexplained failure.
#[test]
#[allow(clippy::too_many_lines)] // one corpus, one ordered list of assertions
fn the_layout_holds_at_five_thousand_files() {
    let tree = polis_repo::synthetic::repository(5_000, GATE_SEED);
    let started = std::time::Instant::now();
    let city = city::generate_city(&tree);
    let elapsed = started.elapsed();
    let structure = city::measure(&city);
    let r = city.report;
    println!("POLIS_SCALE report={r:?}");
    println!("POLIS_SCALE structure={structure:?}");
    println!("POLIS_SCALE generated in {elapsed:?}");

    // Connectivity and planarity: the whole reason this architecture replaced
    // space colonisation.
    assert_eq!(r.components, 1, "the city is in {} pieces", r.components);
    assert_eq!(r.dangling, 0, "a road ends in mid-air");
    assert_eq!(r.crossings, 0, "roads cross without a junction");
    assert_eq!(r.blocks, r.cycles);
    assert!(r.blocks > 700, "only {} blocks: {r:?}", r.blocks);

    // The organic signature: four- and five-way junctions, which is what the eye
    // reads as "grown" (PRD §7.2). The bake-off baseline was 53.3 %; measured
    // with the commit-time ramp it is 46.7 % here, 55.2 % on Neovim and 48.1 %
    // on Django. The share moves with the grain distribution — a repository
    // whose ground is mostly one age tiles more regularly — so the bound is set
    // below all three rather than at the fixture's own number. Two fifths is
    // still far above what a snapped tree or a chord partition produces, and the
    // properties that say "not a tree" (cycles, no dangling) are asserted
    // separately and absolutely.
    assert!(
        r.complex_junctions * 5 >= r.junctions * 2,
        "only {} of {} junctions are four-way or better",
        r.complex_junctions,
        r.junctions
    );

    // Buildings (baseline: 1.6 % in the road corridor, 0.3 % outside their lot,
    // 8.9 % coverage).
    assert_eq!(structure.buildings_on_road, 0);
    assert_eq!(structure.buildings_outside_lot, 0);
    assert!(
        structure.coverage > 0.25,
        "only {:.1}% of the ground is built on",
        structure.coverage * 100.0
    );

    // Block geometry (bake-off baseline: 0 slivers, p95:p05 6.8x, median
    // compactness 0.722, age gradient 1.96x).
    assert!(structure.compactness > 0.65, "{}", structure.compactness);

    // # Both of these are now properties of the repository, not of the generator
    //
    // The age ramp reads real commit time (PRD §7.1, `polis_layout::age`), so a
    // repository with a large founding cohort has a large fine-grained core and
    // a steep gradient, and one that rewrote itself has neither. Measured on
    // three corpora with the same code:
    //
    //   | corpus            | first-year files | p95:p05 | age gradient |
    //   |-------------------|------------------|---------|--------------|
    //   | Neovim, 3 890     | 36.3 %           | 23.8x   | 5.96x        |
    //   | this fixture      |  8.2 %           | 10.0x   | 2.07x        |
    //   | Django, 7 014     |  3.4 %           |  5.2x   | 1.65x        |
    //
    // The previous 20.2x and 2.98x came from ramping on *growth-sequence
    // position*, which spreads every repository's ages evenly over the ramp
    // whatever its history — a number the generator manufactured rather than
    // measured. These bounds are set below the fixture and above Django on
    // purpose: the gate's job is to catch a generator that stopped producing a
    // gradient at all, not to require every repository to have an old town.
    // `the_age_ramp_follows_real_commit_time` is where the ramp's *response* to
    // history is asserted, and it is the stronger test.
    assert!(
        structure.block_hierarchy > 8.0,
        "block size hierarchy is only {:.1}x",
        structure.block_hierarchy
    );
    assert!(
        structure.age_gradient > 1.9,
        "age gradient is only {:.2}x",
        structure.age_gradient
    );

    // Through-streets: the design bake-off's second named defect, and its
    // acceptance range. **Both bounds matter.** Under 35 % of the city diameter
    // the plan is the soap foam the bake-off rejected; over 70 % it is
    // `treemap-arterials`' city-spanning boulevard chord, which it also
    // rejected. And one long stroke on its own proves nothing, so the count of
    // strokes past a quarter of the diameter is asserted with it: a city has a
    // *hierarchy* of through-streets, a foam with one boulevard in it does not.
    //
    // Measured on this corpus: longest 46 %, 16 through-streets, against 42 %
    // and 13 before the avenues — and, more to the point, a median sinuosity
    // (arc length over end-to-end reach) of 1.00 for the ten longest against
    // 1.07 before. The strokes are now geometrically straight rather than
    // chains of wiggles that happen to end up far apart.
    assert!(
        structure.longest_stroke > 0.35 && structure.longest_stroke < 0.70,
        "longest stroke is {:.1}% of the diameter; the acceptance band is 35-70%",
        structure.longest_stroke * 100.0
    );
    assert!(
        structure.through_streets >= 8,
        "only {} strokes reach a quarter of the city diameter",
        structure.through_streets
    );

    // Is it a place, or a diagram? These are the two numbers three M1 gates in
    // a row were failed on, and neither was in this file — they were in a
    // reviewer's notebook, which is why the answer to a geometric failure was a
    // tonal fix, three times.
    //
    // Solidity was 0.9947–0.9994 across five corpora while the city limit was a
    // convex polygon. A circle is 1.00. Anything under 0.90 has a coastline.
    assert!(
        structure.solidity < 0.90,
        "the city fills {:.1}% of its own convex hull: that is a coin, not a coast",
        structure.solidity * 100.0
    );
    // And there were four to nine dead-straight district borders running from
    // inside 5 % of the radius out past 90 % of it, within two degrees of
    // radial: the pie chart. Zero, not a rate — one such border is a drawn
    // avenue and the eye finds it immediately.
    assert_eq!(
        structure.radial_spokes, 0,
        "{} district borders run from the middle of the city to its edge on a radial bearing",
        structure.radial_spokes
    );
    assert_eq!(
        structure.radial_strokes, 0,
        "{} through-streets pass through the civic square and out the other side",
        structure.radial_strokes
    );
    // And the same question with the bearing taken out of it, because the two
    // above only ask whether the ruler pointed at the middle. The competing
    // attempt in this round scored `radial_spokes == 0` with a dead-straight
    // border still running 60 % of the way across the city: it had replaced a
    // radial chord partition with a non-radial one. A border made of Voronoi
    // bisectors measures 8-10 % here.
    assert_eq!(
        structure.straight_borders, 0,
        "{} district borders are dead straight for more than a fifth of the city (longest {:.1}%): the partition is drawing with a ruler",
        structure.straight_borders,
        structure.straight_border * 100.0
    );

    // Districts. The bake-off baseline was 102 of 276 in more than one piece,
    // and the first port of the territory graft still had 41 of 307. Both were
    // tuning misses; the number this gate holds is **zero**, because
    // `polis_layout::districts`' two rules make a district's blocks one
    // edge-connected region by construction (see that module for the argument).
    // Any fragment at all means the construction failed, not that a weight
    // needs adjusting.
    assert_eq!(
        r.fragmented_districts, 0,
        "{} of {} districts are in more than one piece",
        r.fragmented_districts, r.districts
    );
    assert_eq!(
        r.fragmented_packages, 0,
        "{} top-level packages are in more than one piece",
        r.fragmented_packages
    );
    assert!(
        r.fragmented_subtrees * 8 <= r.districts,
        // A rate, and the loosest of the three contiguity numbers on purpose.
        // The two the PRD names are absolute and hold: no district in more than
        // one piece and no top-level package either, on every corpus measured.
        // An *intermediate* directory is a union of districts, and `regions`
        // guarantees connectivity where it is bought — at the top level, so a
        // package is one piece — and lets the balance choose below it, where a
        // sibling's parcel can land between two branches of the same middling
        // directory. Measured: 15 of 237 at 3 000 files and 25 of 312 at 5 000,
        // against 102 of 276 districts in the design bake-off's own prototype.
        "{} of {} directory subtrees are in more than one piece",
        r.fragmented_subtrees,
        r.districts
    );
    // The growth's preference for budding a plot onto its own district's
    // ground. It is no longer what makes a district contiguous — `regions`
    // partitions the plot adjacency graph and every part of that partition is
    // connected by construction — so this is a **shaping** number: how much
    // work the partition has to do, not whether it worked. Measured 123 of 955
    // plots here. A third is twice that, and a growth that had stopped
    // preferring its own ground would sit near the foreign share of the
    // frontier, well past it.
    assert!(
        r.settled_nonadjacent * 5 <= r.plots,
        "{} of {} plots founded out of contact with their own district",
        r.settled_nonadjacent,
        r.plots
    );
    // There is no district polygon any more, so there is no fringe of one to
    // settle on: the growth is free and `regions` partitions afterwards. The
    // field is kept and asserted at **zero** rather than deleted, because a
    // non-zero value would mean some polygon had come back.
    assert_eq!(
        r.settled_on_fringe, 0,
        "{} plots settled on the fringe of a polygon that should not exist",
        r.settled_on_fringe
    );
    assert_eq!(r.relaxed_to_ancestor, 0);
    assert_eq!(r.relaxed_to_anywhere, 0);
    assert_eq!(r.detached_placements, 0);
    // Districts that had to seat their files on a sibling's ground because the
    // subtree they are in was given fewer plots than it has directories. A
    // **rate**, and a small one: the partition divides plot *capacity*, so a
    // subtree of many tiny directories can come out one plot short of one plot
    // each even though the growth settled enough. `regions` restores the floor
    // by moving plots back across the cut, which takes it from 7 to 2 of 312
    // here and to 0 on Django, Neovim and CPython. The files are still placed —
    // `unhoused` and the overflow below are zero — they are just on a
    // neighbour's parcel.
    assert!(
        r.shared_faces * 100 <= r.districts,
        "{} of {} districts had to share a sibling's ground",
        r.shared_faces,
        r.districts
    );
    assert_eq!(r.faceless_districts, 0);

    // Every plot has a cell and every cell has a face. Both are zero by
    // construction and both were **not** on a real repository: Django's leaf
    // quarters stopped tiling the city limit, 1 453 of 2 413 plots came out with
    // an empty cell, and 2 006 files ended up sharing a parcel four stages
    // later. `polis_layout::territory::Quarter` records the bug and
    // `covers_the_rim` is the net under it; these two are how the gate says so.
    assert_eq!(
        r.empty_cells, 0,
        "{} plots have no Voronoi cell at all",
        r.empty_cells
    );
    assert_eq!(
        r.plots_off_face, 0,
        "{} plots landed inside no face and were attached to a stranger's block",
        r.plots_off_face
    );
    assert_eq!(r.overflow, 0, "{} files had to share a parcel", r.overflow);

    // PRD §13.1: cold start to first frame under 3 s for a 5 000-file repo, and
    // generation is only part of that.
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "generation took {elapsed:?}"
    );
}

/// PRD §9's district rule, at four scales and on both fixture repositories.
///
/// > A **District** is a directory […] the tree determines placement, because
/// > directory paths are the addressing system already in use in every tool
/// > call, error message, and conversation.
///
/// A district in two pieces is not addressed by its path, and PRD §8's skeleton
/// is unreadable where it happens. The single-scale gate above could be met by a
/// corpus that happens to suit the partition; this one says the property is not
/// a coincidence. Zero at 200, 1 000, 3 000 and 5 000 files, and zero on the two
/// pinned fixtures.
#[test]
fn every_district_is_one_place_on_the_map_at_every_scale() {
    let mut checked = 0;
    for name in ["hamlet", "town"] {
        let r = fixture_city(name).report;
        assert_eq!(
            r.fragmented_districts, 0,
            "{name}: {} of {} districts are in more than one piece",
            r.fragmented_districts, r.districts
        );
        assert_eq!(r.fragmented_packages, 0, "{name}");
        checked += 1;
    }
    for files in [200_usize, 1_000, 3_000, 5_000] {
        let city = city::generate_city(&polis_repo::synthetic::repository(files, GATE_SEED));
        let r = city.report;
        println!(
            "POLIS_DISTRICTS files={files} districts={} fragmented={} packages={} \
             subtrees={} presplit={} nonadjacent={} fringe={} ancestor={}",
            r.districts,
            r.fragmented_districts,
            r.fragmented_packages,
            r.fragmented_subtrees,
            r.presplit_districts,
            r.settled_nonadjacent,
            r.settled_on_fringe,
            r.relaxed_to_ancestor
        );
        assert_eq!(
            r.fragmented_districts, 0,
            "{files} files: {} of {} districts are in more than one piece",
            r.fragmented_districts, r.districts
        );
        // A package is a *subtree*, and subtree contiguity is not structural for
        // the reason spelled out below: a cousin's Voronoi cell can reach across
        // the chord two sibling faces share. At every scale that matters it is
        // nevertheless zero — 0 at 1 000, 3 000 and 5 000 files, 0 on Neovim's
        // 3 890 and 0 on Django's 7 014 — but a two-hundred-file corpus has
        // thirteen packages and thirty-two districts, so a single cell reaching
        // one block too far is 8 % of the map. Measured: 1 at 200 and 250 files,
        // 0 at 150, 300 and 400.
        let allowed = usize::from(files < 1_000);
        assert!(
            r.fragmented_packages <= allowed,
            "{files} files: {} top-level packages are in more than one piece",
            r.fragmented_packages
        );
        // "Adjacent directories are adjacent on the ground" at *every* level of
        // the tree, not only at the leaves and the root's children.
        //
        // This one is a bound rather than a zero, and the difference is honest
        // rather than convenient. A district being one piece is structural —
        // rules T and A give it. A whole subtree being one piece is not: two
        // sibling districts are given faces that share a chord, but nothing
        // forces a plot on one side of that chord to be a Voronoi neighbour of a
        // plot on the other, so a cousin's cell can reach across it. Measured
        // here: 0, 1, 4 and 2 subtrees of 32, 144, 238 and 312.
        //
        // Requiring a district's *founding* plot to touch its parent's ground
        // was tried and is worse on measurement, not better: at 1 000 files it
        // took fragmented districts from 0 to 1, packages from 0 to 1 and
        // rule-A bends from 0 to 18, because the founding plot is exactly the
        // one with the least room to manoeuvre. Recorded in ADR-0058.
        assert!(
            r.fragmented_subtrees * 8 <= r.districts,
            "{files} files: {} of {} directory subtrees are in more than one piece",
            r.fragmented_subtrees,
            r.districts
        );
        // The growth's sibling preference, as a shaping number rather than a
        // guarantee: `fragmented_districts` above is the guarantee, and
        // `regions` delivers it on the graph. Measured 1 of 61, 33 of 345 and
        // 123 of 955 plots at 200, 1 000 and 5 000 files.
        assert!(
            r.settled_nonadjacent * 5 <= r.plots,
            "{files} files: {} of {} plots founded out of contact with their own district",
            r.settled_nonadjacent,
            r.plots
        );
        assert_eq!(r.settled_on_fringe, 0);
        // Nothing may be placed with no legal position at all: the packing
        // distance and the connectivity reach are hard, and a plot that
        // satisfied neither would be a plot the road graph cannot reach.
        assert_eq!(
            r.relaxed_to_ancestor + r.relaxed_to_anywhere + r.detached_placements,
            0,
            "{files} files: a plot settled outside its own district"
        );
        assert!(
            r.settled_on_fringe * 50 <= r.plots,
            "{files} files: {} of {} plots settled over their own border",
            r.settled_on_fringe,
            r.plots
        );
        // The road network is unharmed by constraining where plots may land.
        assert_eq!(r.components, 1, "{files} files: the city is in pieces");
        assert_eq!(r.crossings, 0, "{files} files");
        assert_eq!(r.dangling, 0, "{files} files");
        assert!(
            r.complex_junctions * 100 >= r.junctions * 45,
            "{files} files: only {} of {} junctions are four-way or better",
            r.complex_junctions,
            r.junctions
        );
        checked += 1;
    }
    assert_eq!(checked, 6, "the corpus shrank");
}

// PRD §13.1's incremental budget lives in `tests/incremental_budget.rs`, not
// here. `cargo test` runs the tests inside one binary in parallel, and several
// of the tests in this file generate a five-thousand-file city — so timing a
// growth step here was timing the harness: 36 ms median alone against 42 ms
// median racing this binary, with a p95 that crossed the budget. Cargo runs test
// *targets* sequentially, so a target holding one test measures the step.
//
/// Adding a file does not move the city out from under the operator.
///
/// > **Never move the ground while the operator is looking at it.** (PRD §7.7)
///
/// The honest claim, and it is weaker than "a grown city equals a generated
/// one": the territory partition is computed from the whole file list, so a
/// repository that has grown by six files has a slightly different partition
/// from one that always had them, and the quantised weights make that *rare*
/// rather than impossible. What is asserted here is what PRD §7.7 actually asks
/// for — that the overwhelming majority of the map is untouched by one add —
/// together with every structural invariant still holding afterwards.
#[test]
fn adding_a_file_leaves_the_rest_of_the_city_where_it_was() {
    let full = polis_repo::synthetic::repository(600, GATE_SEED);
    let newest: LogicalPath = {
        let mut by_age: Vec<(&LogicalPath, u32)> = full
            .files
            .iter()
            .map(|(p, m)| (p, m.growth_index))
            .collect();
        by_age.sort_by_key(|(p, g)| (*g, (*p).clone()));
        by_age.last().expect("a file").0.clone()
    };
    let mut partial = full.clone();
    partial.files.remove(&newest);

    let mut city = city::generate_city(&partial);
    let before: BTreeMap<LogicalPath, Vec<(f32, f32)>> = city
        .layout
        .buildings
        .iter()
        .map(|(p, b)| {
            (
                p.clone(),
                b.footprint.vertices.iter().map(|v| (v.x, v.y)).collect(),
            )
        })
        .collect();

    city.accrete(
        &full,
        &LayoutInputs::default(),
        std::slice::from_ref(&newest),
    );

    assert!(
        city.building(&newest).is_some(),
        "the new file got no building"
    );
    let mut same = 0usize;
    for (path, footprint) in &before {
        if city.building(path).is_some_and(|b| {
            b.footprint
                .vertices
                .iter()
                .map(|v| (v.x, v.y))
                .eq(footprint.iter().copied())
        }) {
            same += 1;
        }
    }
    println!(
        "POLIS_STABILITY {same} of {} buildings unmoved by one add",
        before.len()
    );
    assert!(
        same * 10 >= before.len() * 9,
        "one added file moved {} of {} buildings",
        before.len() - same,
        before.len()
    );

    // And every invariant still holds afterwards.
    let r = city.report;
    assert_eq!(r.components, 1);
    assert_eq!(r.dangling, 0);
    assert_eq!(r.crossings, 0);
    assert_eq!(
        r.blocks,
        (r.road_segments + r.components).saturating_sub(r.road_nodes)
    );
    let structure = city::measure(&city);
    assert_eq!(structure.buildings_on_road, 0);
    assert_eq!(structure.buildings_outside_lot, 0);
}

/// Two fresh processes agree about a 3 000-file synthetic city.
///
/// The fixture version of this test above covers 90 files. Scale is where a
/// nondeterminism actually hides: a tie broken by iteration order needs two
/// candidates that tie, and small inputs rarely produce one.
#[test]
fn two_fresh_processes_agree_at_scale() {
    let mine = synthetic_digest();
    for i in 0..2 {
        assert_eq!(
            child_synthetic_digest(),
            mine,
            "child process {i} produced a different city at 3 000 files"
        );
    }
}

/// The digest [`two_fresh_processes_agree_at_scale`] compares.
fn synthetic_digest() -> u64 {
    city::generate_city(&polis_repo::synthetic::repository(3_000, GATE_SEED)).digest()
}

/// Re-invokes this binary and reads the digest back.
fn child_synthetic_digest() -> u64 {
    let exe = std::env::current_exe().expect("test binary path");
    let output = Command::new(exe)
        .args([
            "--exact",
            "print_synthetic_digest",
            "--ignored",
            "--nocapture",
        ])
        // Pinned, not inherited: see `child_markers`.
        .env("RUST_TEST_THREADS", "1")
        .output()
        .expect("re-invoke the test binary");
    assert!(output.status.success(), "child process failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    child_markers(&stdout, "POLIS_SYNTHETIC_DIGEST ")
        .first()
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .unwrap_or_else(|| panic!("child printed no digest:\n{stdout}"))
}

/// The child half of [`two_fresh_processes_agree_at_scale`].
#[test]
#[ignore = "child process of two_fresh_processes_agree_at_scale"]
fn print_synthetic_digest() {
    println!("POLIS_SYNTHETIC_DIGEST {:016x}", synthetic_digest());
}

/// The input file order cannot move the city, at scale and in every profile.
///
/// `RepoTree::files` is a `BTreeMap`, so the pipeline is handed its files in
/// path order however they were collected; this pushes them through a `HashMap`
/// first, which is the strongest adversary Rust offers, and does it on a corpus
/// big enough for ties to exist.
#[test]
fn file_order_cannot_move_the_city_at_scale() {
    let tree = polis_repo::synthetic::repository(1_500, GATE_SEED);
    let reference = city::generate_city(&tree).digest();
    let shuffled: HashMap<LogicalPath, FileMeta> = tree
        .files
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut rebuilt = RepoTree {
        files: BTreeMap::new(),
        ..tree.clone()
    };
    for (path, meta) in shuffled {
        rebuilt.files.insert(path, meta);
    }
    assert_eq!(
        city::generate_city(&rebuilt).digest(),
        reference,
        "the city moved when its files arrived in `RandomState` order"
    );
}

/// The rendered PNG is a pure function of the layout.
///
/// PRD §15 M1 ends in "render it to a window (or PNG)". A determinism gate that
/// stops at the layout leaves the last step — the one an operator actually looks
/// at — unproven, and the design bake-off found a real leak there: a wall-clock
/// number printed in the legend.
#[test]
fn the_rendered_plan_is_reproducible() {
    let tree = polis_repo::synthetic::repository(400, GATE_SEED);
    let city = city::generate_city(&tree);
    let structure = city::measure(&city);
    let first =
        polis_render::plan::render_plan(&city, &structure, "GATE", 300, 1, true).encode_png();
    let second =
        polis_render::plan::render_plan(&city, &structure, "GATE", 300, 1, true).encode_png();
    assert_eq!(first, second, "the PNG moved between two renders");
    let junctions =
        polis_render::plan::render_junctions(&city, &structure, "GATE", 300, 1).encode_png();
    assert_eq!(
        junctions,
        polis_render::plan::render_junctions(&city, &structure, "GATE", 300, 1).encode_png()
    );
    assert!(
        first.len() > 1_000,
        "the plan encoded to {} bytes",
        first.len()
    );
}
