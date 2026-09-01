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
const TOWN_HEAD: &str = "3b99d29aae0c36d175feaa95cc078b6623de6a30";

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
        .output()
        .expect("re-invoke the test binary");
    assert!(
        output.status.success(),
        "child process failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Vec<(String, u64)> = stdout
        .lines()
        .filter_map(|line| line.trim().strip_prefix("POLIS_CITY_DIGEST "))
        .filter_map(|rest| {
            let (name, digest) = rest.split_once('=')?;
            Some((
                name.to_owned(),
                u64::from_str_radix(digest.trim(), 16).ok()?,
            ))
        })
        .collect();
    assert_eq!(parsed.len(), 2, "child printed no digests:\n{stdout}");
    parsed
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
/// convenience. Both fixtures produce `streets == []`: their import graphs have
/// no cross-district edge, so all three golden files pin an empty `streets`
/// array. That is precisely why this bug survived — a golden file that never
/// contained a street cannot notice the streets being reordered. Until a fixture
/// grows a cross-district import, the real repository is the only input on this
/// machine that exercises the field at all, so the assertion below refuses to
/// pass vacuously.
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
