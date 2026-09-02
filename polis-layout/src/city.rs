//! The pipeline, the growth step, and the serialized [`CityLayout`].
//!
//! # The order is non-negotiable
//!
//! > Roads → blocks → lots → buildings. This is the Parish & Müller ordering
//! > from the CityEngine line of work. Place buildings first and connect them
//! > afterward and you get suburbia or a circuit board, every time. (PRD §7.2)
//!
//! It is preserved here, and it is *tested* rather than assumed: blocks are
//! recovered by an actual half-edge face traversal of the road graph, so the
//! block count is not "one per plot" and cannot silently become that.
//!
//! # The seven stages
//!
//! | Stage | Module | What it produces |
//! |---|---|---|
//! | 1 terrain | [`crate::terrain`] | the height field roads follow (PRD §7.2 step 1) |
//! | 2 regions | `regions` | one connected set of plots per district, from the tree |
//! | 3 accretion | `accrete` | settled plots, replayed in git commit order (PRD §7.1) |
//! | 4 roads | `voronoi`, [`crate::roads`] | the Voronoi boundary network, welded, collapsed, pruned |
//! | 5 blocks | [`crate::blocks`] | the closed faces of that network |
//! | 6 lots | [`crate::lots`] | strip subdivision, seated frontage first |
//! | 7 buildings | [`crate::buildings`] | oriented footprints, provably off the road |
//!
//! Stage 2 is a **constraint, not a road generator**. The recursive partition
//! decides where a district's ground *may* be; it never draws a line that ends
//! up on the map. Every road on the map is a Voronoi boundary between two
//! accreted plots.
//!
//! # Why the previous pipeline was replaced
//!
//! Space colonisation scattered attractor clouds on a golden-angle spiral. PRD
//! §7.2's snap can only bridge what is already close, so it never fired across
//! the gaps between clouds: the graph came out as one small tree per island,
//! with no closed faces, therefore no blocks, therefore degenerate lots and
//! degenerate buildings. Growing the settlement instead — and taking the roads
//! as the boundaries of the settled ground — makes cycles a property of the
//! construction rather than of a tuning constant.

// The middle of the pipeline is numeric geometry, and five lint families fire on
// nearly every line of it without telling us anything:
//
// * the `cast_*` family — every cast here lands in a bucket index or a quantised
//   sort key that is clamped or wrapped on purpose;
// * `float_cmp` — exact float comparison is how a determinism tie is broken
//   (PRD §7.4), and an approximate comparison there would be the bug;
// * `many_single_char_names` and `similar_names` — `a`, `b`, `c`, `n`, `p` are
//   the names the geometry itself uses;
// * `too_many_lines` — a pipeline stage read as one ordered sequence is clearer
//   than the same code cut into fragments each called once;
// * `assigning_clones` — the buffers reassigned here are rebuilt from scratch,
//   so `clone_from` would save nothing.
#![allow(
    clippy::assigning_clones,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines
)]

use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::time::Duration;

use polis_events::{LogicalPath, WallTime};
use polis_repo::tree::EntryPointKind;
use polis_repo::{FileClass, RepoDelta, RepoTree};
use serde::{Serialize, Serializer};

use rayon::prelude::*;

use crate::accrete::{self, FileRec, Params, Settlement};
use crate::age::AgeRamp;
use crate::blocks::{self, district_of, BlockPlan};
use crate::buildings::{self, BuildingSpec};
use crate::determinism::{quantize, quantize_point, QUANTUM};
use crate::districts;
use crate::geom::{self, add, area, centroid, dist, dot, len, mul, sub, to_point, to_polygon, Pt};
use crate::lots::{self, Overflow, VacancyLedger};
use crate::regions;
use crate::roads::{self, Graph};
use crate::terrain::TerrainField;
use crate::territory;
use crate::voronoi::{self, Cells};
use crate::{
    Block, BlockId, Building, CityLayout, District, Lot, LotId, Point, Polygon, RoadClass,
    RoadGraph, RoofForm, StreetLine, LAYOUT_SCHEMA,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Version of the serialized snapshot format (PRD §16).
///
/// Bumped whenever the *shape* changes, so a stale golden file fails loudly
/// rather than diffing line by line.
pub const SNAPSHOT_FORMAT: u32 = 3;

/// Decimals every coordinate is printed at.
pub const SNAPSHOT_DECIMALS: u32 = 3;

/// The terrain seed.
///
/// A **constant**, not a hash of the repository path: PRD §15's M1 gate demands
/// a byte-identical layout across two machines, and two machines check the same
/// repository out at different absolute paths.
pub const TERRAIN_SEED: u64 = 0x91d5_1b0b_7c25_dbd8;

/// Resolution of the terrain digest grid.
pub const TERRAIN_DIGEST_RESOLUTION: u32 = 16;

/// At most this many monuments (PRD §8).
pub const MAX_MONUMENTS: usize = 24;

/// Shortest layout tween (PRD §7.7).
pub const MIN_TWEEN_MS: u64 = 800;

/// Longest layout tween.
pub const MAX_TWEEN_MS: u64 = 4_000;

/// Tween milliseconds added per changed file.
pub const TWEEN_MS_PER_CHANGE: u64 = 20;

/// How long the camera must be still before a deformation is applied.
pub const CAMERA_STILL_MS: u64 = 250;

/// How many times a batch may be deferred before it is applied anyway.
pub const MAX_DEFERRALS: u32 = 32;

/// How large a batch may get before it is applied regardless of the camera.
pub const MAX_DEFERRED_CHANGES: usize = 64;

/// Turn angle, as a cosine, above which a stroke keeps going straight.
///
/// Used only for measurement: the stroke length distribution is the number that
/// says whether the city has through-streets or is a soap foam.
///
/// **`cos 40°`, the standard natural-road continuation rule.** It used to be
/// `0.55`, which is `cos 56.6°`, and the difference is not cosmetic: at 56.6°
/// the rule chains through a wiggle that no driver would call one road, and it
/// reported a 76 %-of-diameter through-street on a network whose junction render
/// has no straight line in it anywhere. Measured on the same graph, the same
/// morning: 76.1 % at 56.6°, 75.6 % at 40°, 46.4 % at 30° — and 12, 5 and 4
/// strokes past a quarter of the city. A measurement that flatters the thing it
/// measures is worse than no measurement, so this is pinned at the convention.
pub const STROKE_TURN_COS: f64 = 0.766;

/// How straight a chain of district border has to be to count as *drawn*.
///
/// Two degrees. Not a judgement call: the previous partition's wedge boundaries
/// measured 0.2–2.7 degrees off radial over 46 % of the city diameter, because
/// they were exactly straight lines and only the weld's rounding bent them.
pub const RADIAL_STRAIGHT_COS: f64 = 0.999_390;

/// How near radial such a chain has to be to count as a spoke, in radians.
pub const RADIAL_TOLERANCE: f64 = 0.087_266; // 5 degrees

/// Shortest such chain that counts, as a share of the city diameter.
pub const RADIAL_MIN_SHARE: f64 = 0.10;

/// How much of the city a dead-straight district border has to cross before it
/// reads as a **drawn line** rather than as a coincidence of two plots.
///
/// A fifth of the diameter. The three artefact partitions measured 46–93 %
/// (radial wedges) and 54–60 % (non-radial chords); a border made of Voronoi
/// bisectors measures 8–10 %, so there is an order of magnitude between the two
/// populations and this threshold sits in the gap rather than beside either.
pub const STRAIGHT_BORDER_SHARE: f64 = 0.20;

/// A spoke starts inside this share of the radius…
pub const RADIAL_INNER: f64 = 0.20;

/// …and reaches past this one.
pub const RADIAL_OUTER: f64 = 0.60;

/// How close to the civic square a stroke passes before it counts as through it.
pub const CIVIC_APPROACH: f64 = 0.05;

/// How opposite two bearings have to be to be one boulevard: 150 degrees.
pub const OPPOSITE_COS: f64 = -0.866;

/// A stroke this long, as a share of the city diameter, is a through-street.
pub const THROUGH_STREET_SHARE: f64 = 0.25;

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// Everything the layout wants that the repository tree does not carry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutInputs {
    /// Cross-district import relations (PRD §9).
    pub streets: Vec<polis_repo::imports::Street>,
    /// Inbound import counts, for PRD §8's monument ranking.
    pub inbound: Vec<(LogicalPath, u32)>,
    /// Uncommitted diff lines per file, which drive height (PRD §7.3).
    pub diff_lines: BTreeMap<LogicalPath, u32>,
}

impl LayoutInputs {
    /// Diff lines for one file, zero when unknown.
    #[must_use]
    pub fn diff_lines_of(&self, path: &LogicalPath) -> u32 {
        self.diff_lines.get(path).copied().unwrap_or(0)
    }
}

/// The terrain field's parameters and a digest of the field itself.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct TerrainParams {
    /// The seed.
    pub seed: u64,
    /// Half-width of the field's domain.
    pub extent: f32,
    /// Octaves of fbm.
    pub octaves: u32,
    /// Peak relief.
    pub relief: f32,
    /// How many districts biased the field.
    pub districts: usize,
    /// A digest of the sampled field, so a change to it shows in the snapshot.
    pub digest: u64,
}

impl TerrainParams {
    /// Pin a field.
    #[must_use]
    pub fn of(field: &TerrainField) -> Self {
        Self {
            seed: field.seed(),
            extent: field.extent(),
            octaves: field.octaves(),
            relief: field.relief(),
            districts: field.district_count(),
            digest: field.digest(TERRAIN_DIGEST_RESOLUTION),
        }
    }
}

/// One of PRD §8's orientation anchors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonumentMark {
    /// The file.
    pub path: LogicalPath,
    /// Rank, strongest anchor first.
    pub rank: u32,
    /// Which kind of entry point it is, if that is why it is a monument.
    pub entry: Option<EntryPointKind>,
    /// Inbound import count.
    pub inbound: u32,
    /// True when it is in the top decile of inbound imports.
    pub top_decile: bool,
}

/// One of PRD §8's industrial zones, drawn as a single dull mass.
#[derive(Debug, Clone, PartialEq)]
pub struct IndustrialMass {
    /// The directory.
    pub district: LogicalPath,
    /// The district whose ground it is drawn on.
    pub host: LogicalPath,
    /// How many files it stands for.
    pub files: u32,
    /// How many bytes.
    pub bytes: u64,
    /// The shape to draw.
    pub boundary: Polygon,
}

/// What the generator produced, in numbers.
///
/// Every field is a structural property that a regression would move, which is
/// why the whole struct is serialized into the golden file rather than printed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct CityReport {
    /// Files in the tree.
    pub files: usize,
    /// Buildings placed.
    pub buildings: usize,
    /// Blocks: the closed faces of the road graph.
    pub blocks: usize,
    /// Blocks with no plot inside — squares, greens, undeveloped ground.
    pub open_blocks: usize,
    /// Blocks that are too thin or far too small next to the median.
    pub slivers: usize,
    /// Parcels surveyed.
    pub lots: usize,
    /// Parcels nobody was assigned to: yards and gardens (PRD §7.5).
    pub empty_lots: usize,
    /// Files deliberately unhoused because PRD §8 masses them.
    pub massed: usize,
    /// Files that had to share a parcel.
    pub overflow: usize,
    /// Files with a parcel but no building.
    pub unbuilt: usize,
    /// Settled plots.
    pub plots: usize,
    /// Road nodes.
    pub road_nodes: usize,
    /// Road segments.
    pub road_segments: usize,
    /// Connected components of the road graph. **One** is the design's promise.
    pub components: usize,
    /// Independent cycles, `E − V + C`. Equal to the bounded face count.
    pub cycles: usize,
    /// Nodes of degree other than two.
    pub junctions: usize,
    /// Nodes of degree four or more.
    pub complex_junctions: usize,
    /// Nodes of degree one. A grown network has none.
    pub dangling: usize,
    /// Proper crossings without a node. Zero, by construction.
    pub crossings: usize,
    /// Districts with ground.
    pub districts: usize,
    /// Districts whose blocks fall into more than one connected group.
    pub fragmented_districts: usize,
    /// Directory subtrees, at every level of the tree, whose blocks fall into
    /// more than one connected group.
    ///
    /// "Adjacent directories are adjacent on the ground", measured: a subtree is
    /// cut out of one face, so `src/` should be one quarter and `src/auth/` one
    /// neighbourhood inside it. Strictly stronger than
    /// [`Self::fragmented_districts`], which is only the leaf case, and than
    /// [`Self::fragmented_packages`], which is only the root's children.
    pub fragmented_subtrees: usize,
    /// Top-level packages whose blocks fall into more than one connected group.
    ///
    /// The number that decides whether the map is readable: colour is keyed on
    /// the package, so a package in two pieces is a package the eye cannot find.
    pub fragmented_packages: usize,
    /// Faces the territory partition could not divide, so two or more districts
    /// share ground. Every one of these is a chance for a district to fragment.
    pub shared_faces: usize,
    /// Districts with files that the partition never gave ground of their own.
    pub faceless_districts: usize,
    /// Districts whose Voronoi cells were in more than one piece before a single
    /// road was welded or contracted — a failure of the growth rules themselves,
    /// which no later repair could fix. **Zero** at every scale measured.
    pub presplit_districts: usize,
    /// Plots that settled inside their own polygon but out of contact with the
    /// rest of their district's ground.
    ///
    /// The only placement left that can split a district in two, so it is the
    /// number that says how far "districts are contiguous by construction" is
    /// from literally true. See `crate::districts`.
    pub settled_nonadjacent: usize,
    /// Plots that settled on the fringe of their own polygon.
    pub settled_on_fringe: usize,
    /// Plots that had to settle in an ancestor district's polygon.
    pub relaxed_to_ancestor: usize,
    /// Plots that had to settle anywhere inside the city limit.
    pub relaxed_to_anywhere: usize,
    /// Plots founded out of contact with the settlement. **Zero**, or the road
    /// graph gains a second component.
    pub detached_placements: usize,
    /// Files whose plot fell in no block at all, so they got no lot.
    pub unhoused: usize,
    /// Plots that landed inside **no** face of the road graph and had to be
    /// attached to the nearest block instead.
    ///
    /// Not the same as [`Self::unhoused`], and worse: an unhoused file is
    /// counted and drawn as such, while a plot off its face is silently
    /// **added** to a block it is not in, taking that block's lots away from the
    /// files that really live there. It is the number that explains a large
    /// [`Self::overflow`], and on a real repository of thousands of two-file
    /// directories it was the whole of it.
    pub plots_off_face: usize,
    /// Plots whose Voronoi cell came out empty, so the plot contributes no
    /// ground to the tiling. Every one of these becomes a
    /// [`Self::plots_off_face`].
    pub empty_cells: usize,
    /// Median block area on the newest ground over the oldest, times 100.
    ///
    /// PRD §7.1's age gradient, measured rather than asserted: the old town
    /// should have a finer mesh and smaller blocks than the periphery. An
    /// integer, so [`CityReport`] stays `Eq` and a golden file cannot drift by
    /// one bit of an `f64`.
    pub age_gradient_x100: u32,
    /// Monuments (PRD §8).
    pub monuments: usize,
    /// Which regime PRD §7.1's age ramp is in for this repository.
    ///
    /// In the golden file because it decides how the whole map reads: a
    /// repository that slid from `calibrated` to `uniform` because its history
    /// was squashed is a different city, and it should say so rather than
    /// showing up as an unexplained diff in every block area.
    pub age_ramp: crate::age::AgeRampKind,
    /// Days between the first and the last file addition in git.
    pub history_days: u32,
    /// Files added inside the repository's **first year** — PRD §7.1's old town,
    /// counted rather than assumed.
    pub old_town_files: usize,
    /// Files whose ground lands in the core band — PRD §7.1's old town as the
    /// operator actually sees it.
    ///
    /// Equal to [`Self::old_town_files`] on a repository whose growth curve
    /// suits the calendar ramp, larger on one that does not, and the difference
    /// is what [`Self::age_equalisation_x100`] bought.
    pub core_files: usize,
    /// How far the age ramp had to be corrected away from pure commit time
    /// toward the repository's own distribution, in hundredths.
    ///
    /// `0` is a repository PRD §7.1 describes literally. `100` is one whose
    /// calendar says nothing usable about where its core is. See
    /// `crate::age::AgeRamp::equalisation`; an integer here because the report
    /// is compared byte-for-byte in a golden file.
    pub age_equalisation_x100: u32,
}

// ---------------------------------------------------------------------------
// The city
// ---------------------------------------------------------------------------

/// The accretion state a city carries so growth stays incremental.
///
/// > Growth is genuinely incremental — a new file runs one growth step, it does
/// > not regenerate the world. (PRD §7.4)
///
/// This is what makes that literally true here: adding a file calls
/// `Settlement::add_file` once, which is the same call the batch generator
/// makes, so there is no second code path to keep in sync.
#[derive(Debug, Clone, Default)]
pub struct Growth {
    /// The settled ground.
    pub(crate) settlement: Settlement,
    /// The Voronoi territories of the plots.
    pub(crate) cells: Cells,
    /// The terrain field, pinned so a growth step reproduces it exactly.
    pub(crate) terrain: TerrainField,
}

impl Growth {
    /// The road corridor half-width this city was laid out with.
    ///
    /// One number, read by the pipeline and by the measurement, so a building
    /// cannot be clear of the road by one definition and inside it by another.
    #[must_use]
    pub fn road_half(&self) -> f64 {
        self.settlement.params.sep_core * roads::ROAD_HALF
    }
}

/// The generated city: [`CityLayout`] plus everything PRD §8 needs that does not
/// fit in it.
#[derive(Debug, Clone, Default)]
pub struct City {
    /// The layout proper — roads, blocks, lots, buildings, districts, streets.
    pub layout: CityLayout,
    /// The never-rendered height field's parameters and digest.
    pub terrain: TerrainParams,
    /// PRD §8's monuments, strongest anchor first.
    pub monuments: Vec<MonumentMark>,
    /// PRD §8's industrial zones, in district order.
    pub industrial: Vec<IndustrialMass>,
    /// PRD §7.5's record of which lots were built on and then emptied.
    pub vacancies: VacancyLedger,
    /// Which district each district's files were routed to.
    pub host_of: BTreeMap<LogicalPath, LogicalPath>,
    /// Files that had to share a parcel. Never dropped; the renderer must draw
    /// them.
    pub overflow: Vec<Overflow>,
    /// What happened, in numbers.
    pub report: CityReport,
    /// The accretion state, so one more file is one more growth step.
    pub growth: Growth,
    /// Block subdivisions carried over from the previous growth step.
    ///
    /// A **timing** structure and nothing else: every hit is checked bit for bit
    /// against the arguments that produced it, so the cache can skip work but
    /// cannot change an output (PRD §7.4). It is deliberately outside
    /// [`CitySnapshot`] and outside [`City::digest`] — two cities with the same
    /// map are the same city whatever either one remembered on the way there.
    pub(crate) cuts: crate::memo::CutCache,
    /// Buildings carried over from the previous growth step. See
    /// [`City::cuts`]; the same rules apply, and for the same reason.
    pub(crate) seats: crate::memo::SeatCache,
}

impl Default for TerrainParams {
    fn default() -> Self {
        Self::of(&TerrainField::default())
    }
}

impl City {
    /// The golden-file serialization (PRD §16).
    pub fn snapshot(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&CitySnapshot::of(self))
    }

    /// A 64-bit digest of the snapshot, for a cheap equality check.
    #[must_use]
    pub fn digest(&self) -> u64 {
        self.snapshot()
            .map(|s| crate::determinism::fnv1a64(s.as_bytes()))
            .unwrap_or_default()
    }

    /// The building standing on a file's lot.
    #[must_use]
    pub fn building(&self, path: &LogicalPath) -> Option<&Building> {
        self.layout.buildings.get(path)
    }

    /// Empties what the growth step remembers, so the next one recomputes
    /// everything from scratch.
    ///
    /// The caches behind [`City::reuse`] are *timing* structures whose every hit
    /// is verified bit for bit against the arguments that produced it, so
    /// forgetting them must change nothing but the clock — which is what
    /// `a_warm_cache_and_a_cold_one_build_the_same_city` asserts. Exposed
    /// because a city that will not grow again can give the memory back.
    pub fn forget(&mut self) {
        self.cuts = crate::memo::CutCache::default();
        self.seats = crate::memo::SeatCache::default();
    }

    /// What the last assembly reused: `(block cuts, seated buildings)`, each the
    /// share in `[0, 1]` that did not have to be recomputed.
    ///
    /// `(0, 0)` for a city generated from scratch, which had nothing to reuse.
    /// A growth step that reports low numbers is one that moved the ground, and
    /// PRD §7.7 says it should not have.
    #[must_use]
    pub fn reuse(&self) -> (f64, f64) {
        (
            self.cuts.counts().hit_rate(),
            self.seats.counts().hit_rate(),
        )
    }

    /// True when a file is one of PRD §8's anchors.
    #[must_use]
    pub fn is_monument(&self, path: &LogicalPath) -> bool {
        self.monuments.iter().any(|m| &m.path == path)
    }

    /// Applies one growth step, keeping the PRD §7.5 vacancy record.
    ///
    /// `when` is the commit time of the change, supplied by the caller —
    /// nothing in `polis-layout` reads a clock (PRD §7.4).
    pub fn grow(
        &mut self,
        tree: &RepoTree,
        delta: &RepoDelta,
        inputs: &LayoutInputs,
        when: WallTime,
    ) -> GrowthOutcome {
        let outcome = step(
            &mut self.layout,
            &mut self.vacancies,
            tree,
            delta,
            inputs,
            when,
        );
        self.report.buildings = self.layout.buildings.len();
        self.report.empty_lots = self.layout.lots.iter().filter(|l| l.is_vacant()).count();
        outcome
    }

    /// One **accretion** growth step: settle the new files on real ground and
    /// rebuild the neighbourhood they touched.
    ///
    /// Unlike [`City::grow`], which seats a file on ground that already exists,
    /// this runs the same `Settlement::add_file` the batch generator runs, and
    /// then the same downstream. There is no second pipeline to keep in sync.
    ///
    /// It is **not** claimed that the result is byte-identical to generating the
    /// larger repository from scratch, and it is not: the territory partition is
    /// computed from the whole file list, so a repository that has grown by a
    /// file has a marginally different partition from one that always had it.
    /// Quantised weights make that rare rather than impossible (PRD §7.7), and
    /// the gate measures what is actually promised — that one add leaves the
    /// overwhelming majority of the map exactly where it was.
    ///
    /// Returns how many road nodes moved, which is the number PRD §7.7 cares
    /// about.
    pub fn accrete(
        &mut self,
        tree: &RepoTree,
        inputs: &LayoutInputs,
        added: &[LogicalPath],
    ) -> usize {
        if added.is_empty() {
            return 0;
        }
        let before: BTreeSet<(i64, i64)> = self
            .layout
            .roads
            .nodes
            .iter()
            .map(|n| {
                (
                    (f64::from(n.position.x) * 1_000.0) as i64,
                    (f64::from(n.position.y) * 1_000.0) as i64,
                )
            })
            .collect();
        let sep = self.growth.settlement.params.sep_rim;
        for path in crate::determinism::canonical_order(added.iter().cloned()) {
            let Some(meta) = tree.file(&path) else {
                continue;
            };
            let plots_before = self.growth.settlement.plots.len();
            let rec = file_record(meta, false);
            self.growth.settlement.add_file(rec);
            if self.growth.settlement.plots.len() > plots_before {
                let at = self.growth.settlement.plots[plots_before].pos;
                let positions = self.growth.settlement.positions();
                // The phantom ring is part of the diagram, so a new plot can
                // retire a phantom that used to bound the edge of town; the
                // cells those phantoms touched have to move with them.
                let moved = voronoi::update_phantoms(&mut self.growth.cells, &positions, at, sep);
                let mut touched = vec![at];
                touched.extend(moved);
                let which = voronoi::affected(&positions, &touched, sep);
                voronoi::rebuild_subset(&positions, sep, &mut self.growth.cells, &which);
            }
        }
        let rebuilt = assemble(
            std::mem::take(&mut self.growth),
            tree,
            inputs,
            self.vacancies.clone(),
            std::mem::take(&mut self.cuts),
            std::mem::take(&mut self.seats),
        );
        *self = rebuilt;
        let after: BTreeSet<(i64, i64)> = self
            .layout
            .roads
            .nodes
            .iter()
            .map(|n| {
                (
                    (f64::from(n.position.x) * 1_000.0) as i64,
                    (f64::from(n.position.y) * 1_000.0) as i64,
                )
            })
            .collect();
        before.symmetric_difference(&after).count()
    }
}

// ---------------------------------------------------------------------------
// Generation (PRD §7.2)
// ---------------------------------------------------------------------------

/// Generates a city from scratch by replaying the growth sequence.
#[must_use]
pub fn generate(tree: &RepoTree) -> CityLayout {
    generate_city(tree).layout
}

/// [`generate`], keeping PRD §8's landmark layer and the terrain pin.
#[must_use]
pub fn generate_city(tree: &RepoTree) -> City {
    generate_with(tree, &LayoutInputs::default())
}

/// [`generate_city`] with streets, monuments and building heights.
#[must_use]
pub fn generate_with(tree: &RepoTree, inputs: &LayoutInputs) -> City {
    generate_with_seed(tree, inputs, TERRAIN_SEED)
}

/// [`generate_with`] with a caller-chosen terrain seed.
///
/// The seed **must be machine-independent and stable across commits**, or PRD
/// §15's M1 gate fails for a reason that has nothing to do with this crate.
#[must_use]
pub fn generate_with_seed(tree: &RepoTree, inputs: &LayoutInputs, seed: u64) -> City {
    let monument_set = rank_monuments(tree, &inputs.inbound);
    let files: Vec<FileRec> = tree
        .files
        .values()
        .map(|meta| file_record(meta, monument_set.contains(&meta.path)))
        .collect();
    let params = Params::for_file_count(files.len());

    // --- 0. The age ramp (PRD §7.1) ---------------------------------------
    // Real commit time, not position in the file ordering. Calibrated once,
    // here, and then read by the partition *and* the accretion: a district that
    // was given ground sized at one grain and settled at another is exactly the
    // failure `districts::demands` documents. See `crate::age`.
    let ramp = AgeRamp::calibrate(
        &files
            .iter()
            .map(|f| (f.growth_index, f.added_at))
            .collect::<Vec<_>>(),
    );

    // --- 1. Terrain (PRD §7.2 step 1) -------------------------------------
    let extent = roads::suggested_extent(tree);
    let terrain = TerrainField::generate(seed, extent);

    // --- 2. The district tree ---------------------------------------------
    let territory = territory::build(&districts::demands(&files), &|path| {
        is_industrial_district(tree, path)
    });

    // --- 3. Accretion (PRD §7.1) ------------------------------------------
    // Free growth: the only hard rules are the packing distance and the
    // connectivity reach. Nothing is laid out in advance, so the town's outline
    // is the outline of the ground people settled.
    let settlement = accrete::grow(files, territory, params, ramp, terrain);

    // --- 4. Roads: the Voronoi of the settled ground ----------------------
    let positions = settlement.positions();
    let cells = voronoi::build(&positions, params.sep_rim, seed ^ 0x7E7E);

    let growth = Growth {
        terrain: settlement.terrain.clone(),
        settlement,
        cells,
    };
    assemble(
        growth,
        tree,
        inputs,
        VacancyLedger::new(),
        crate::memo::CutCache::default(),
        crate::memo::SeatCache::default(),
    )
}

/// Everything downstream of the settled ground.
///
/// Split out because [`City::accrete`] runs exactly this after a local Voronoi
/// update: one code path, so a grown city and a generated one cannot diverge.
#[allow(clippy::too_many_lines)] // the pipeline is one ordered sequence; splitting it hides the order
fn assemble(
    growth: Growth,
    tree: &RepoTree,
    inputs: &LayoutInputs,
    vacancies: VacancyLedger,
    mut cuts: crate::memo::CutCache,
    mut seats: crate::memo::SeatCache,
) -> City {
    let params = growth.settlement.params;

    // Weld the cell corners into a planar graph, then apply PRD §7.2's snap
    // where it actually bridges something.
    let (mut graph, cell_edges) =
        Graph::from_cells_indexed(&growth.cells.cells, roads::WELD_TOLERANCE);

    // --- 2. Districts: a connected partition of the plot adjacency graph ---
    // Not of the plane. See `regions` for why that distinction is the whole of
    // the difference between a place and a pie chart.
    let adjacency = regions::adjacency(&cell_edges, graph.edges.len());
    let seating = regions::partition(&growth.settlement, &adjacency);
    let seated = growth.settlement.reseated(&seating);
    let s = &seated;
    // One boundary per pair of neighbouring cells of the same district is marked
    // before the collapse and never contracted: without it, fusing the single
    // boundary a two-parcel district shares leaves its halves touching at a
    // point, and the district comes out in two pieces. See `districts`.
    let cell_district: Vec<u32> = s.plots.iter().map(|p| p.district).collect();
    let (keep_links, presplit) = districts::links_to_keep(
        &cell_edges,
        &cell_district,
        &s.territory,
        &|e| graph.edge_len(e),
        graph.edges.len(),
    );
    // The collapse threshold follows the *local* separation, so the coarse rim
    // fuses its junctions as readily as the fine core does. A single global
    // threshold fires almost only in the old town, and the four- and five-way
    // junctions -- the organic signature -- never reach the periphery.
    // An avenue is a straight line only for as long as its junctions stay on it,
    // and the collapse puts a merged node at the mean of the chain it
    // contracted. `avenue_masks` says which line each junction is on, and
    // `collapse_short` uses it to keep the line: see its documentation.
    graph.collapse_short(&keep_links, &|p| {
        let t = s.age_at(p);
        params.sep_at(t) * roads::collapse_ratio(t)
    });
    graph.compact_nodes();

    // Prune a minority of interior boundaries so parcels merge into larger,
    // irregular blocks and the age gradient becomes block size.
    let first_pass = graph.faces();
    let face_district = blocks::face_districts(&first_pass, s);
    let border = blocks::border_edges(&first_pass, &face_district, graph.edges.len());
    let doomed = roads::choose_prunes(
        &graph,
        &first_pass,
        &border,
        &|p| s.age_at(p),
        params.terrain_seed ^ 0xA7A7,
    );
    graph.delete_edges(&doomed);
    graph.compact_nodes();
    let faces = graph.faces();
    roads::classify_by_betweenness(&mut graph, roads::BETWEENNESS_SAMPLES);

    // --- 5. Blocks (PRD §7.2 step 3) --------------------------------------
    let (mut block_plans, _plot_block, plots_off_face) = blocks::assign(faces, s);
    // The last word on district shape, taken on the blocks the metric reads.
    blocks::heal_districts(&mut block_plans, &s.territory);
    let package_of = districts::package_of(&s.territory);
    let block_plans = block_plans;
    let civic = blocks::civic_square(&block_plans, s);
    let border_edges = district_borders(&block_plans, graph.edges.len());
    roads::promote_borders(&mut graph, &border_edges);

    // --- 6. Lots (PRD §7.2 step 4) ----------------------------------------
    let road_half = growth.road_half();
    let parcelling = lots::parcel_city(&block_plans, s, civic, road_half, &mut cuts);
    // The repository's own median file size, which the absolute footprint cap is
    // measured against (`buildings::FOOTPRINT_REFERENCE_BYTES`). Computed from
    // the files that will actually carry a building, so a `node_modules` full of
    // minified bundles cannot decide the scale of the source city.
    let median_bytes = median_file_size(s);

    // --- 7. Buildings (PRD §7.2 step 5, §7.3, §8) -------------------------
    // # Why this one stage is parallel and the rest of the pipeline is not
    //
    // Seating a building is the only stage that is a *pure function of one
    // parcel*: the footprint is derived from the parcel ring, the block ring
    // around it and a seed hashed from the file's own path, and it reads nothing
    // any other parcel writes. Everything upstream is a graph the next step
    // mutates.
    //
    // It is also, by a factor of two, the most expensive: measured at 5 000
    // files, `place_in_parcel` over 5 474 parcels is **30.4 ms** of a 58 ms
    // assembly, and assembly is what PRD §13.1's 50 ms incremental budget is
    // spent on — `City::accrete` re-runs exactly this after one growth step.
    //
    // The same purity is why the stage is *skipped* wherever nothing moved: a
    // growth step is asked for the buildings of five thousand parcels and only a
    // handful of them are new. `crate::memo::SeatCache` holds the previous
    // step's answers and every hit is checked bit for bit against the arguments
    // that produced it, so the cache removes work without being able to change
    // an output. Parallelism alone was not enough: it hides the cost on a
    // twenty-four-core developer machine and returns in full on a four-core CI
    // runner, which is exactly where the budget was flaking.
    // One shared copy of each block's ring: a block has many parcels and each
    // of their keys needs it, and copying it per parcel cost more than the
    // seating it saves.
    let block_rings: Vec<(std::sync::Arc<Vec<crate::geom::Pt>>, u64)> = block_plans
        .iter()
        .map(|b| {
            (
                std::sync::Arc::new(b.ring.clone()),
                crate::memo::digest_ring(&b.ring),
            )
        })
        .collect();
    let seat_inputs: Vec<(Lot, Option<crate::memo::SeatKey>)> = parcelling
        .parcels
        .into_iter()
        .enumerate()
        .map(|(i, parcel)| {
            let id = LotId(u32::try_from(i).expect("lot count fits in u32"));
            let occupant = parcel.occupant.map(|f| s.files[f as usize].path.clone());
            let boundary = to_polygon(&parcel.ring);
            let lot = Lot {
                id,
                block: BlockId(parcel.block),
                boundary,
                occupant: occupant.clone(),
            };
            let key = occupant.and_then(|path| {
                let meta = tree.file(&path)?;
                let rec = &s.files[parcel.occupant.expect("occupied") as usize];
                let spec = BuildingSpec::new(rec.size_bytes)
                    .with_diff_lines(inputs.diff_lines_of(&path))
                    .with_size_reference(median_bytes)
                    .with_class(if rec.monument {
                        FileClass::Monument
                    } else {
                        meta.class
                    });
                let (block, block_digest) = &block_rings[parcel.block as usize];
                Some(crate::memo::SeatKey {
                    parcel: parcel.ring,
                    block: std::sync::Arc::clone(block),
                    block_digest: *block_digest,
                    path,
                    spec,
                    lot: id,
                    road_half,
                })
            });
            (lot, key)
        })
        .collect();

    // What the previous growth step already seated, on a parcel that has not
    // moved since. Read-only and single-threaded, so the cache itself is never
    // shared across the `rayon` fan-out below (`crate::memo`).
    let mut known: Vec<Option<Option<Building>>> = seat_inputs
        .iter()
        .map(|(_, key)| key.as_ref().and_then(|k| seats.get(k)))
        .collect();
    let todo: Vec<usize> = (0..seat_inputs.len())
        .filter(|&i| seat_inputs[i].1.is_some() && known[i].is_none())
        .collect();
    // Determinism (PRD §7.4, and rule 4 in this crate's module docs) survives
    // because the results are collected **in index order** into a `Vec` and only
    // then folded. No thread writes to a shared map, nothing is pushed as it
    // finishes, and the fold is a sequential loop over that `Vec`. The
    // `BTreeMap` is filled afterwards and is ordered by key regardless.
    let fresh: Vec<Option<Building>> = todo
        .par_iter()
        .map(|&i| {
            let k = seat_inputs[i].1.as_ref().expect("filtered to occupied");
            buildings::place_in_parcel(&k.parcel, &k.block, &k.path, k.spec, k.lot, k.road_half)
        })
        .collect();

    let mut lot_list: Vec<Lot> = Vec::with_capacity(seat_inputs.len());
    let mut buildings_map: BTreeMap<LogicalPath, Building> = BTreeMap::new();
    let mut unbuilt = 0usize;
    let mut computed = todo.iter().copied().zip(fresh).peekable();
    let mut next_seats = crate::memo::SeatCache::with_capacity(known.len());
    for (i, (lot, key)) in seat_inputs.into_iter().enumerate() {
        // A parcel with no occupant, or one whose occupant the tree no longer
        // holds, has no key and is not counted as a building that failed to
        // stand — it is a parcel that was never asked for one.
        let (building, hit) = if let Some(hit) = known[i].take() {
            (hit, true)
        } else if let Some((_, building)) = computed.next_if(|(j, _)| *j == i) {
            (building, false)
        } else {
            (None, false)
        };
        if let Some(key) = key {
            if let (Some(path), Some(building)) = (lot.occupant.as_ref(), building.as_ref()) {
                buildings_map.insert(path.clone(), building.clone());
            }
            unbuilt += usize::from(building.is_none());
            next_seats.put(key, building, hit);
        }
        lot_list.push(lot);
    }
    seats = next_seats;
    // --- Districts, streets, landmarks ------------------------------------
    let district_path = |d: u32| s.territory.nodes[d as usize].path.clone();
    let block_list = blocks::publish(&block_plans, &district_path);
    let districts = build_districts(&block_plans, &block_list, s);
    let streets = build_streets(&inputs.streets, &districts, &graph);
    let monuments = monument_marks(tree, &inputs.inbound, &buildings_map);
    let host_of = build_host_of(tree, &districts);
    let industrial = industrial_masses(tree, &host_of, &districts, &block_list);

    let road_graph = graph.to_road_graph();
    let stats = roads::junction_stats(&road_graph);
    let crossings = roads::crossings(&road_graph).len();
    // The city's extent is the ground it covers: the furthest a drawn cell
    // corner gets from the origin. There is no city limit polygon to read it off
    // any more, and that is the point.
    let extent = crate::determinism::narrow(crate::determinism::quantize_f64(
        block_plans
            .iter()
            .flat_map(|b| b.ring.iter())
            .map(|p| dist(*p, [0.0, 0.0]))
            .fold(1.0f64, f64::max),
    ));

    // Deeper directories are higher ground (PRD §7.2 step 1). Applied to a copy
    // *after* the growth, never to the field the growth read: a terrain the
    // growth cannot see is a terrain that cannot make an incrementally grown
    // city differ from a generated one.
    let mut relief = growth.terrain.clone();
    for (d, node) in s.territory.nodes.iter().enumerate() {
        if !s.ground[d].plots.is_empty() {
            relief.bias_district(&node.path, to_point(s.ground[d].centroid()));
        }
    }

    let report = CityReport {
        files: tree.files.len(),
        buildings: buildings_map.len(),
        blocks: block_list.len(),
        open_blocks: block_plans.iter().filter(|b| b.open).count(),
        slivers: blocks::sliver_count(&block_plans),
        lots: lot_list.len(),
        empty_lots: lot_list.iter().filter(|l| l.is_vacant()).count(),
        massed: parcelling.report.massed,
        overflow: parcelling.report.overflow,
        unbuilt,
        plots: s.plots.len(),
        road_nodes: stats.nodes,
        road_segments: stats.segments,
        components: stats.components,
        cycles: stats.cycles,
        junctions: stats.junctions,
        complex_junctions: stats.complex_junctions,
        dangling: stats.dangling,
        crossings,
        districts: districts.len(),
        fragmented_districts: districts::fragmented(&block_plans, &|d| Some(d)),
        fragmented_packages: districts::fragmented(&block_plans, &|d| package_of.get(&d).copied()),
        fragmented_subtrees: districts::fragmented_subtrees(&block_plans, &s.territory),
        shared_faces: seating.shared,
        faceless_districts: seating.faceless,
        presplit_districts: presplit,
        settled_nonadjacent: s.relaxed.nonadjacent,
        settled_on_fringe: s.relaxed.fringe,
        relaxed_to_ancestor: s.relaxed.ancestor,
        relaxed_to_anywhere: s.relaxed.anywhere,
        detached_placements: s.relaxed.detached,
        unhoused: parcelling.report.unplaced,
        plots_off_face,
        empty_cells: growth.cells.empty(),
        age_gradient_x100: age_gradient(&block_plans),
        monuments: monuments.len(),
        age_ramp: s.ramp.kind(),
        history_days: s.ramp.span_days(),
        old_town_files: s.ramp.old_town(),
        core_files: s.ramp.core_files(),
        // `round`, then a saturating cast: the value is in `[0, 1]` by
        // construction, so this is exact on every target (PRD §7.4).
        age_equalisation_x100: (s.ramp.equalisation() * 100.0).round() as u32,
    };

    let layout = CityLayout {
        schema: LAYOUT_SCHEMA,
        extent,
        roads: road_graph,
        blocks: block_list,
        lots: lot_list,
        buildings: buildings_map,
        districts,
        streets,
    };

    City {
        layout,
        terrain: TerrainParams::of(&relief),
        monuments,
        industrial,
        vacancies,
        host_of,
        overflow: parcelling.overflow,
        report,
        growth,
        cuts,
        seats,
    }
}

/// The median size of the files that will carry a building.
///
/// PRD §8's massed trees are excluded: `node_modules` is an order of magnitude
/// larger per file than hand-written source and it is drawn as one shape anyway,
/// so letting it set the scale would shrink every real building in the city.
///
/// The lower median of two equally-sized halves, never an average of the middle
/// pair — an integer answer that cannot differ in the last bit between targets
/// (PRD §7.4).
fn median_file_size(s: &Settlement) -> u64 {
    let mut sizes: Vec<u64> = s
        .files
        .iter()
        .filter(|f| !f.industrial)
        .map(|f| f.size_bytes)
        .collect();
    if sizes.is_empty() {
        return crate::buildings::FOOTPRINT_REFERENCE_BYTES as u64;
    }
    sizes.sort_unstable();
    sizes[(sizes.len() - 1) / 2].max(1)
}

/// One [`FileRec`] from the index.
fn file_record(meta: &polis_repo::FileMeta, monument: bool) -> FileRec {
    FileRec {
        path: meta.path.clone(),
        size_bytes: meta.size_bytes.max(1),
        growth_index: meta.growth_index,
        added_at: meta.added_at,
        industrial: meta.class.is_massed(),
        monument: monument || meta.class == FileClass::Monument,
    }
}

/// True when every file under a directory is one PRD §8 masses.
fn is_industrial_district(tree: &RepoTree, district: &LogicalPath) -> bool {
    let mut any = false;
    for meta in tree.files.values() {
        if district.is_root() || meta.path.starts_with(district) {
            if !meta.class.is_massed() {
                return false;
            }
            any = true;
        }
    }
    any && !district.is_root()
}

/// Edges with a different district on each side, or a district on exactly one.
///
/// These are the borders PRD §8 wants readable at every zoom, and they are road
/// edges rather than a separate overlay — a district boundary in this city *is*
/// a street.
fn district_borders(blocks: &[BlockPlan], edges: usize) -> Vec<usize> {
    let mut side: Vec<[i64; 2]> = vec![[-1, -1]; edges];
    for b in blocks {
        let d = b.district.map_or(-2, i64::from);
        for &h in &b.half_edges {
            let e = h / 2;
            if e < side.len() {
                side[e][h % 2] = d;
            }
        }
    }
    (0..edges)
        .filter(|&e| {
            let [a, b] = side[e];
            (a >= 0 && b >= 0 && a != b) || ((a >= 0) != (b >= 0))
        })
        .collect()
}

/// Median block area on the newest ground over the oldest, times 100.
///
/// Blocks are ranked by the settlement step of their oldest plot, which is the
/// literal age of the ground — not by distance from the centre, which would beg
/// the question.
fn age_gradient(blocks: &[BlockPlan]) -> u32 {
    let mut aged: Vec<(u32, f64)> = blocks
        .iter()
        .filter(|b| b.birth != u32::MAX)
        .map(|b| (b.birth, b.area()))
        .collect();
    if aged.len() < 6 {
        return 100;
    }
    aged.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));
    let third = aged.len() / 3;
    let median_of = |slice: &[(u32, f64)]| -> f64 {
        let mut v: Vec<f64> = slice.iter().map(|(_, a)| *a).collect();
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let core = median_of(&aged[..third]);
    let rim = median_of(&aged[aged.len() - third..]);
    if core <= 0.0 {
        return 100;
    }
    ((rim / core) * 100.0)
        .round()
        .clamp(0.0, f64::from(u32::MAX)) as u32
}

/// One [`District`] per district that has ground, in path order.
///
/// The boundary is the convex hull of the district's own blocks. It is a label
/// anchor and a coarse extent, not a drawn outline: the district's real border
/// is the set of road edges between one of its blocks and a stranger's, which is
/// what the renderer draws and what `districts::fragmented` measures.
fn build_districts(
    plans: &[BlockPlan],
    published: &[Block],
    s: &Settlement,
) -> BTreeMap<LogicalPath, District> {
    let mut by_district: BTreeMap<u32, Vec<BlockId>> = BTreeMap::new();
    for (i, plan) in plans.iter().enumerate() {
        if let Some(d) = plan.district {
            by_district.entry(d).or_default().push(published[i].id);
        }
    }
    let mut out = BTreeMap::new();
    for (d, mut ids) in by_district {
        ids.sort_unstable();
        let node = &s.territory.nodes[d as usize];
        let ring = geom::convex_hull(
            &ids.iter()
                .flat_map(|id| plans[id.0 as usize].ring.iter().copied())
                .collect::<Vec<Pt>>(),
        );
        let ground = &s.ground[d as usize];
        let centre = if ground.plots.is_empty() {
            centroid(&ring)
        } else {
            ground.centroid()
        };
        out.insert(
            node.path.clone(),
            District {
                path: node.path.clone(),
                boundary: to_polygon(&ring),
                centre: to_point(centre),
                blocks: ids,
            },
        );
    }
    out
}

/// Cross-district import relations, routed along the roads (PRD §9).
///
/// > Streets are **cross-district import relationships only** […] Do not let the
/// > import graph fight the directory tree for position.
///
/// Dijkstra over the road graph with arterials discounted, so a street prefers a
/// main road exactly as traffic does, and **no street is a chord across open
/// ground**. The frontier is keyed on a quantised cost then the node id, so two
/// equal-cost paths resolve the same way on every machine.
fn build_streets(
    relations: &[polis_repo::imports::Street],
    districts: &BTreeMap<LogicalPath, District>,
    graph: &Graph,
) -> Vec<StreetLine> {
    let mut out = Vec::new();
    if graph.nodes.is_empty() {
        return out;
    }
    // Canonical order first: `relations` is a caller-supplied `Vec` and its
    // order must not reach the layout, in any build profile (PRD §7.4).
    let mut ordered: Vec<(LogicalPath, LogicalPath, u32)> = relations
        .iter()
        .map(|r| (r.from.clone(), r.to.clone(), r.edge_count))
        .collect();
    ordered.sort();
    ordered.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

    let anchor = |path: &LogicalPath| -> Option<u32> {
        districts
            .get(path)
            .and_then(|d| roads::nearest_node_index(graph, geom::from_point(d.centre)))
    };
    let mut by_source: BTreeMap<LogicalPath, Vec<(LogicalPath, u32)>> = BTreeMap::new();
    for (from, to, edges) in ordered {
        if districts.contains_key(&from) && districts.contains_key(&to) {
            by_source.entry(from).or_default().push((to, edges));
        }
    }
    let n = graph.nodes.len();
    for (from, targets) in by_source {
        let Some(source) = anchor(&from) else {
            continue;
        };
        let mut cost = vec![f64::INFINITY; n];
        let mut prev = vec![u32::MAX; n];
        let mut heap: BinaryHeap<(std::cmp::Reverse<i64>, std::cmp::Reverse<u32>)> =
            BinaryHeap::new();
        cost[source as usize] = 0.0;
        heap.push((std::cmp::Reverse(0), std::cmp::Reverse(source)));
        while let Some((std::cmp::Reverse(dq), std::cmp::Reverse(u))) = heap.pop() {
            let du = dq as f64 * 1e-5;
            if du > cost[u as usize] + 1e-9 {
                continue;
            }
            for &e in &graph.adj[u as usize] {
                let v = graph.other(e as usize, u);
                let discount = match graph.class[e as usize] {
                    RoadClass::Arterial => 0.72,
                    RoadClass::Street => 0.88,
                    RoadClass::Alley => 1.0,
                };
                let nd = du + graph.edge_len(e as usize) * discount;
                if nd + 1e-9 < cost[v as usize] {
                    cost[v as usize] = nd;
                    prev[v as usize] = e;
                    heap.push((std::cmp::Reverse((nd * 1e5) as i64), std::cmp::Reverse(v)));
                }
            }
        }
        for (to, edges) in targets {
            let Some(target) = anchor(&to) else { continue };
            if !cost[target as usize].is_finite() {
                continue;
            }
            let mut polyline: Vec<Point> = Vec::new();
            let mut cur = target;
            let mut guard = 0usize;
            while cur != source && prev[cur as usize] != u32::MAX && guard < n {
                guard += 1;
                polyline.push(to_point(graph.nodes[cur as usize]));
                let e = prev[cur as usize] as usize;
                cur = graph.other(e, cur);
            }
            polyline.push(to_point(graph.nodes[source as usize]));
            polyline.reverse();
            if polyline.len() >= 2 {
                out.push(StreetLine {
                    from: from.clone(),
                    to,
                    edge_count: edges,
                    polyline,
                });
            }
        }
    }
    out.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.to.cmp(&b.to)));
    out
}

/// PRD §9's free diagnostics, read off the street layer.
///
/// > A district with streets to everywhere is a hub or a god-module. One with
/// > none is isolated.
///
/// Both fall out of the layer at no cost, which is the point of drawing imports
/// as streets rather than as a separate graph: the question "which quarter does
/// everything lead to?" is answered by looking at the map.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreetDiagnostics {
    /// Districts with a street to at least a quarter of the other districts:
    /// the hubs and god-modules.
    pub hubs: usize,
    /// Districts with no street at all: nothing imports them and they import
    /// nothing outside their own quarter.
    pub isolated: usize,
    /// Distinct import edges carried by the widest single street.
    pub widest: u32,
}

/// Measure [`StreetDiagnostics`] on a finished city.
#[must_use]
pub fn street_diagnostics(city: &City) -> StreetDiagnostics {
    let districts = city.layout.districts.len();
    if districts == 0 {
        return StreetDiagnostics::default();
    }
    let mut degree: BTreeMap<&LogicalPath, usize> = BTreeMap::new();
    let mut widest = 0u32;
    for line in &city.layout.streets {
        *degree.entry(&line.from).or_insert(0) += 1;
        *degree.entry(&line.to).or_insert(0) += 1;
        widest = widest.max(line.edge_count);
    }
    // "Streets to everywhere" needs a threshold and this is it: a quarter of the
    // other districts. Stated here rather than left to the reader, because a
    // diagnostic whose threshold is implicit is not a diagnostic.
    let hub_at = (districts.saturating_sub(1) / 4).max(1);
    let hubs = degree.values().filter(|d| **d >= hub_at).count();
    StreetDiagnostics {
        hubs,
        isolated: districts - degree.len(),
        widest,
    }
}

/// The road node nearest a point, ties breaking to the lowest index.
#[must_use]
pub fn nearest_node(graph: &RoadGraph, at: Point) -> Option<Point> {
    let mut best: Option<(usize, f32)> = None;
    for (i, node) in graph.nodes.iter().enumerate() {
        let d2 = node.position.distance_squared(at);
        if best.is_none_or(|(_, bd)| d2 < bd) {
            best = Some((i, d2));
        }
    }
    best.and_then(|(i, _)| graph.nodes.get(i))
        .map(|n| n.position)
}

/// The set of files promoted to [`FileClass::Monument`], capped (PRD §8).
fn rank_monuments(tree: &RepoTree, inbound: &[(LogicalPath, u32)]) -> BTreeSet<LogicalPath> {
    polis_repo::tree::monuments(tree, inbound)
        .into_iter()
        .take(MAX_MONUMENTS)
        .map(|m| m.path)
        .collect()
}

/// PRD §8's monument marks, for the monuments that actually got a building.
fn monument_marks(
    tree: &RepoTree,
    inbound: &[(LogicalPath, u32)],
    placed: &BTreeMap<LogicalPath, Building>,
) -> Vec<MonumentMark> {
    let mut out = Vec::new();
    for candidate in polis_repo::tree::monuments(tree, inbound)
        .into_iter()
        .take(MAX_MONUMENTS)
    {
        // A monument with no building is not a landmark; a label floating over
        // nothing is worse than no label.
        if !placed.contains_key(&candidate.path) {
            continue;
        }
        let rank = u32::try_from(out.len()).unwrap_or(u32::MAX);
        out.push(MonumentMark {
            path: candidate.path,
            rank,
            entry: candidate.entry,
            inbound: candidate.inbound,
            top_decile: candidate.top_decile,
        });
    }
    out
}

/// Which district each district's files were drawn on.
fn build_host_of(
    tree: &RepoTree,
    districts: &BTreeMap<LogicalPath, District>,
) -> BTreeMap<LogicalPath, LogicalPath> {
    let mut out = BTreeMap::new();
    for meta in tree.files.values() {
        let district = district_of(&meta.path);
        if out.contains_key(&district) {
            continue;
        }
        let host = ancestor_with_ground(&district, districts).unwrap_or_else(|| {
            districts
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(LogicalPath::root)
        });
        out.insert(district, host);
    }
    out
}

/// One dull shape per industrial tree (PRD §8).
fn industrial_masses(
    tree: &RepoTree,
    host_of: &BTreeMap<LogicalPath, LogicalPath>,
    districts: &BTreeMap<LogicalPath, District>,
    block_list: &[Block],
) -> Vec<IndustrialMass> {
    let mut totals: BTreeMap<LogicalPath, (u32, u64)> = BTreeMap::new();
    for meta in tree.files.values() {
        if !meta.class.is_massed() {
            continue;
        }
        let entry = totals.entry(district_of(&meta.path)).or_insert((0, 0));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = entry.1.saturating_add(meta.size_bytes);
    }
    let fallback = block_list.first().map(|b| b.district.clone());
    totals
        .into_iter()
        .map(|(district, (files, bytes))| {
            let host = host_of
                .get(&district)
                .cloned()
                .or_else(|| ancestor_with_ground(&district, districts))
                .or_else(|| fallback.clone())
                .unwrap_or_else(LogicalPath::root);
            let boundary = districts
                .get(&host)
                .map_or_else(Polygon::default, |d| d.boundary.clone());
            IndustrialMass {
                district,
                host,
                files,
                bytes,
                boundary,
            }
        })
        .collect()
}

/// The nearest ancestor of `district` that has blocks, itself included.
fn ancestor_with_ground(
    district: &LogicalPath,
    districts: &BTreeMap<LogicalPath, District>,
) -> Option<LogicalPath> {
    let mut candidate = Some(district.clone());
    while let Some(path) = candidate {
        if districts.contains_key(&path) {
            return Some(path);
        }
        candidate = path.parent();
    }
    None
}

/// The convex hull of a point set, counter-clockwise (Andrew's monotone chain).
#[must_use]
pub fn convex_hull(points: &[Point]) -> Polygon {
    let ring = geom::convex_hull(
        &points
            .iter()
            .copied()
            .filter(|p| p.is_finite())
            .map(geom::from_point)
            .collect::<Vec<Pt>>(),
    );
    to_polygon(&ring)
}

// ---------------------------------------------------------------------------
// The growth step (PRD §7.4)
// ---------------------------------------------------------------------------

/// What one [`step`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrowthOutcome {
    /// Files that settled onto a lot and got a building.
    pub placed: usize,
    /// Lots emptied by a deletion (PRD §7.5).
    pub vacated: usize,
    /// Buildings rebuilt because their size or their diff changed.
    pub updated: usize,
    /// Files with nowhere to go on the existing lot set.
    pub unplaced: Vec<LogicalPath>,
    /// True when the city needs a full accretion step to house everything.
    pub rebuild_required: bool,
}

impl GrowthOutcome {
    /// True when nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.placed == 0 && self.vacated == 0 && self.updated == 0 && self.unplaced.is_empty()
    }
}

/// Applies one growth step for a repository delta, seating files on ground that
/// already exists.
///
/// Budget: under 50 ms, off-thread (PRD §13.1). This is the cheap path — it
/// never moves a road. When it cannot house a file it says so, and the caller
/// runs [`City::accrete`], which settles new ground.
pub fn grow(layout: &mut CityLayout, tree: &RepoTree, delta: &RepoDelta) {
    let mut ledger = VacancyLedger::new();
    step(
        layout,
        &mut ledger,
        tree,
        delta,
        &LayoutInputs::default(),
        WallTime::UNIX_EPOCH,
    );
}

/// [`grow`], keeping the PRD §7.5 record and reporting what happened.
///
/// The order is deletions, then updates, then additions — deliberately, because
/// a lot freed by a deletion is a candidate for an addition in the same batch,
/// which is how a rename reuses ground instead of pushing the city outwards.
pub fn step(
    layout: &mut CityLayout,
    ledger: &mut VacancyLedger,
    tree: &RepoTree,
    delta: &RepoDelta,
    inputs: &LayoutInputs,
    when: WallTime,
) -> GrowthOutcome {
    let mut outcome = GrowthOutcome::default();
    let road_half = road_half_of(layout);

    for path in crate::determinism::canonical_order(delta.removed.iter().cloned()) {
        layout.buildings.remove(&path);
        if ledger.vacate_path(&mut layout.lots, &path, when).is_some() {
            outcome.vacated += 1;
        }
    }

    let changed = crate::determinism::canonical_order(
        delta.resized.iter().chain(delta.retouched.iter()).cloned(),
    );
    for path in changed {
        if rebuild_building(layout, tree, inputs, &path, road_half) {
            outcome.updated += 1;
        }
    }

    let by_district = lots_by_district(layout);
    for path in &delta.added {
        let candidates = host_candidates(&district_of(path), &by_district);
        let Some(lot_id) = ledger.settle(&mut layout.lots, &candidates, path) else {
            outcome.unplaced.push(path.clone());
            outcome.rebuild_required = true;
            continue;
        };
        if rebuild_building(layout, tree, inputs, path, road_half) {
            outcome.placed += 1;
        } else {
            let _ = lot_id;
        }
    }
    outcome
}

/// The road corridor half-width implied by a layout's own scale.
fn road_half_of(layout: &CityLayout) -> f32 {
    let mut lengths: Vec<f32> = layout
        .roads
        .segments
        .iter()
        .filter_map(|s| {
            let a = layout.roads.nodes.get(s.from.0 as usize)?.position;
            let b = layout.roads.nodes.get(s.to.0 as usize)?.position;
            Some(a.distance(b))
        })
        .collect();
    if lengths.is_empty() {
        return crate::determinism::narrow(roads::ROAD_HALF);
    }
    lengths.sort_by(f32::total_cmp);
    let median = lengths[lengths.len() / 2];
    quantize(median * crate::determinism::narrow(roads::ROAD_HALF))
}

/// Lots of each district, in id order.
fn lots_by_district(layout: &CityLayout) -> BTreeMap<LogicalPath, Vec<LotId>> {
    let district: BTreeMap<BlockId, LogicalPath> = layout
        .blocks
        .iter()
        .map(|b| (b.id, b.district.clone()))
        .collect();
    let mut out: BTreeMap<LogicalPath, Vec<LotId>> = BTreeMap::new();
    for lot in &layout.lots {
        if let Some(path) = district.get(&lot.block) {
            out.entry(path.clone()).or_default().push(lot.id);
        }
    }
    out
}

/// Candidate lots for a district: its own, else its nearest ancestor's.
fn host_candidates(
    district: &LogicalPath,
    by_district: &BTreeMap<LogicalPath, Vec<LotId>>,
) -> Vec<LotId> {
    let mut candidate = Some(district.clone());
    while let Some(path) = candidate {
        if let Some(lots) = by_district.get(&path) {
            return lots.clone();
        }
        candidate = path.parent();
    }
    Vec::new()
}

/// Rebuilds the building standing on a file's lot. `false` when it has none.
fn rebuild_building(
    layout: &mut CityLayout,
    tree: &RepoTree,
    inputs: &LayoutInputs,
    path: &LogicalPath,
    road_half: f32,
) -> bool {
    let Some(meta) = tree.file(path) else {
        return false;
    };
    let Some(lot) = layout
        .lots
        .iter()
        .find(|lot| lot.occupant.as_ref() == Some(path))
    else {
        return false;
    };
    let spec = BuildingSpec::new(meta.size_bytes)
        .with_diff_lines(inputs.diff_lines_of(path))
        .with_class(meta.class);
    if let Some(building) = buildings::place_with(lot, path, spec, road_half) {
        layout.buildings.insert(path.clone(), building);
        true
    } else {
        layout.buildings.remove(path);
        false
    }
}

/// Measured structure, for reporting and for the gate.
///
/// Everything here is derived from a finished [`City`], so it can be recomputed
/// by anyone who has the snapshot — which is exactly what the design bake-off's
/// judge did, and the reason its numbers held up.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Structure {
    /// The report, as generated.
    pub report: CityReport,
    /// Building footprint area as a share of block area — the design bake-off
    /// judge's cross-cutting finding, and the number that decides whether a
    /// render reads as a city or as a diagram.
    pub coverage: f64,
    /// [`Structure::coverage`] over the third of the city nearest the civic
    /// square. A dense historic core runs 30–60 %; the city as a whole is lower
    /// because its edge is fields, which is what the edge of a city is.
    pub coverage_core: f64,
    /// [`Structure::coverage`] over the outermost third.
    pub coverage_rim: f64,
    /// Median share of its own lot that a building covers.
    ///
    /// Separates the two ways the ground can end up empty: too many vacant lots
    /// (this stays high, [`Structure::coverage`] falls) or buildings too small
    /// for the lots they stand on (this falls too).
    pub lot_fill: f64,
    /// Longest natural road stroke, as a share of the city diameter.
    pub longest_stroke: f64,
    /// Natural strokes at least [`THROUGH_STREET_SHARE`] of the city diameter
    /// long.
    ///
    /// The companion to [`Structure::longest_stroke`], and the more honest of
    /// the two on its own: one long stroke in an otherwise uniform mesh is a
    /// boulevard chord, which is the artefact the design bake-off rejected. A
    /// city has a *hierarchy* of through-streets.
    pub through_streets: usize,
    /// Block areas at the 5th and 95th percentiles, and their ratio.
    pub block_p05: f64,
    /// See [`Structure::block_p05`].
    pub block_p95: f64,
    /// See [`Structure::block_p05`].
    pub block_hierarchy: f64,
    /// Median block area.
    pub block_median: f64,
    /// Median block compactness, `4πA / P²`. A square is 0.785.
    pub compactness: f64,
    /// Buildings whose footprint enters the road corridor. Zero by construction.
    pub buildings_on_road: usize,
    /// Buildings with a vertex outside their own lot. Zero by construction.
    pub buildings_outside_lot: usize,
    /// Ratio of median block area at the rim to median block area in the core —
    /// PRD §7.1's age gradient, measured.
    pub age_gradient: f64,
    /// Built ground as a share of its own convex hull: **is the city a coin?**
    ///
    /// A circle is 1.00 and so is any convex outline; a real coastline is well
    /// under it. Three M1 gates in a row were failed on this number, which sat
    /// at 0.9947–0.9994 across five corpora while the renderer was retoned
    /// twice, so it is measured here rather than in a reviewer's notebook.
    ///
    /// The blocks are faces of a planar subdivision, so they tile without
    /// overlapping and their areas sum to the built ground exactly — no
    /// rasterisation, no tolerance.
    pub solidity: f64,
    /// District borders that run from the middle of the city to its edge on a
    /// radial bearing: **is the city a pie chart?**
    ///
    /// A maximal chain of district-border road edges, straight to within two
    /// degrees, reaching at least a tenth of the city diameter, lying within
    /// five degrees of the radial direction at its midpoint, and spanning from
    /// inside a fifth of the radius to beyond three fifths of it. That is the
    /// shape of an avenue radiating from a civic square, and there were four to
    /// nine of them in every city the previous partition laid out.
    pub radial_spokes: usize,
    /// Through-streets that pass within 5 % of the radius of the civic square
    /// with their two ends on opposite bearings from it.
    ///
    /// The other half of the same failure: two avenues that fuse into one
    /// boulevard across the middle of the town. Counted separately because the
    /// partition's own documentation claimed the civic square prevented it, and
    /// the claim was false on all three real repositories.
    pub radial_strokes: usize,
    /// District borders that are simply **drawn with a ruler**, whatever their
    /// bearing: the longest dead-straight chain of border, as a share of the
    /// city diameter.
    ///
    /// [`Self::radial_spokes`] asks whether the borders point at the middle.
    /// That is the *previous* artefact and only the previous artefact, and a
    /// partition that cuts the plane with chords answers it completely by
    /// choosing chords that are not radial: measured on the competing attempt in
    /// this round, radial spokes 0 with a straight border still running 60 % of
    /// the way across the city. The eye reads that as a ruler either way.
    ///
    /// A border here is a chain of Voronoi bisectors between two settled plots,
    /// so it cannot stay straight for long without the plots being in a row.
    /// Measured 8–10 % on every corpus, against 46–93 % for the radial partition
    /// and 54–60 % for the chord one.
    pub straight_border: f64,
    /// How many such chains run past [`STRAIGHT_BORDER_SHARE`] of the diameter.
    ///
    /// **Zero.** One is a drawn line across the map and the eye finds it before
    /// it finds anything else.
    pub straight_borders: usize,
}

/// A uniform grid over road segments, so the road-clearance check is linear in
/// the number of buildings rather than quadratic.
struct SegmentIndex<'a> {
    cell: f64,
    buckets: BTreeMap<(i64, i64), Vec<u32>>,
    segs: &'a [(Pt, Pt)],
}

impl<'a> SegmentIndex<'a> {
    fn build(segs: &'a [(Pt, Pt)]) -> Self {
        let mut lengths: Vec<f64> = segs.iter().map(|(a, b)| dist(*a, *b)).collect();
        lengths.sort_by(f64::total_cmp);
        let cell = if lengths.is_empty() {
            1.0
        } else {
            (lengths[lengths.len() / 2] * 1.5).max(1e-6)
        };
        let mut buckets: BTreeMap<(i64, i64), Vec<u32>> = BTreeMap::new();
        for (i, (a, b)) in segs.iter().enumerate() {
            let id = u32::try_from(i).unwrap_or(u32::MAX);
            let x0 = (a[0].min(b[0]) / cell).floor() as i64;
            let x1 = (a[0].max(b[0]) / cell).floor() as i64;
            let y0 = (a[1].min(b[1]) / cell).floor() as i64;
            let y1 = (a[1].max(b[1]) / cell).floor() as i64;
            for y in y0..=y1 {
                for x in x0..=x1 {
                    buckets.entry((x, y)).or_default().push(id);
                }
            }
        }
        Self {
            cell,
            buckets,
            segs,
        }
    }

    /// Does any vertex of `ring` come within `radius` of a segment?
    fn any_within(&self, ring: &[Pt], radius: f64) -> bool {
        for p in ring {
            let kx = (p[0] / self.cell).floor() as i64;
            let ky = (p[1] / self.cell).floor() as i64;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let Some(list) = self.buckets.get(&(kx + dx, ky + dy)) else {
                        continue;
                    };
                    for &i in list {
                        let (a, b) = self.segs[i as usize];
                        if geom::dist_to_seg(*p, a, b) < radius {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
}

/// Measure a city's structure.
#[must_use]
#[allow(clippy::too_many_lines)] // one measurement per line; splitting hides the set
pub fn measure(city: &City) -> Structure {
    let mut out = Structure {
        report: city.report,
        ..Structure::default()
    };
    let blocks: Vec<Vec<Pt>> = city
        .layout
        .blocks
        .iter()
        .map(|b| geom::from_polygon(&b.boundary))
        .collect();
    let block_area: Vec<f64> = blocks.iter().map(|r| area(r)).collect();
    let total_block: f64 = block_area.iter().sum();
    let building_area: f64 = city
        .layout
        .buildings
        .values()
        .map(|b| area(&geom::from_polygon(&b.footprint)))
        .sum();
    out.coverage = if total_block > 0.0 {
        building_area / total_block
    } else {
        0.0
    };

    // Coverage by ring, because one number over a city whose edge is open
    // country says less than two.
    let lot_of_block: BTreeMap<u32, u32> = city
        .layout
        .lots
        .iter()
        .map(|l| (l.id.0, l.block.0))
        .collect();
    let mut built_per_block: BTreeMap<u32, f64> = BTreeMap::new();
    for b in city.layout.buildings.values() {
        if let Some(block) = lot_of_block.get(&b.lot.0) {
            *built_per_block.entry(*block).or_insert(0.0) +=
                area(&geom::from_polygon(&b.footprint));
        }
    }
    let mut ringed: Vec<(f64, f64, f64)> = city
        .layout
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| {
            (
                dist(centroid(&blocks[i]), [0.0, 0.0]),
                block_area[i],
                built_per_block.get(&b.id.0).copied().unwrap_or(0.0),
            )
        })
        .collect();
    ringed.sort_by(|x, y| x.0.total_cmp(&y.0));
    if ringed.len() >= 6 {
        let third = ringed.len() / 3;
        let ratio = |slice: &[(f64, f64, f64)]| -> f64 {
            let ground: f64 = slice.iter().map(|(_, a, _)| *a).sum();
            let built: f64 = slice.iter().map(|(_, _, b)| *b).sum();
            if ground > 0.0 {
                built / ground
            } else {
                0.0
            }
        };
        out.coverage_core = ratio(&ringed[..third]);
        out.coverage_rim = ratio(&ringed[ringed.len() - third..]);
    }

    let mut sorted = block_area.clone();
    sorted.sort_by(f64::total_cmp);
    if !sorted.is_empty() {
        out.block_p05 = sorted[sorted.len() * 5 / 100];
        out.block_p95 = sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)];
        out.block_median = sorted[sorted.len() / 2];
        out.block_hierarchy = if out.block_p05 > 0.0 {
            out.block_p95 / out.block_p05
        } else {
            0.0
        };
    }
    let mut compact: Vec<f64> = blocks
        .iter()
        .filter(|r| r.len() >= 3)
        .map(|r| {
            let p: f64 = (0..r.len()).map(|i| dist(r[i], r[(i + 1) % r.len()])).sum();
            if p > 0.0 {
                4.0 * std::f64::consts::PI * area(r) / (p * p)
            } else {
                0.0
            }
        })
        .collect();
    compact.sort_by(f64::total_cmp);
    if !compact.is_empty() {
        out.compactness = compact[compact.len() / 2];
    }

    out.age_gradient = f64::from(city.report.age_gradient_x100) / 100.0;

    // Roads, as drawn.
    let segs: Vec<(Pt, Pt)> = city
        .layout
        .roads
        .segments
        .iter()
        .filter_map(|s| {
            let a = city.layout.roads.nodes.get(s.from.0 as usize)?.position;
            let b = city.layout.roads.nodes.get(s.to.0 as usize)?.position;
            Some((geom::from_point(a), geom::from_point(b)))
        })
        .collect();
    // The corridor is the one the pipeline actually kept buildings out of, read
    // off the settlement rather than guessed from the drawn segments. Measuring
    // against a different number is how a green metric hides a real violation.
    let corridor = city.growth.road_half();
    let index = SegmentIndex::build(&segs);
    let lot_ring: BTreeMap<u32, Vec<Pt>> = city
        .layout
        .lots
        .iter()
        .map(|l| (l.id.0, geom::from_polygon(&l.boundary)))
        .collect();
    for b in city.layout.buildings.values() {
        let ring = geom::from_polygon(&b.footprint);
        if index.any_within(&ring, corridor) {
            out.buildings_on_road += 1;
        }
        if let Some(lot) = lot_ring.get(&b.lot.0) {
            if !ring.iter().all(|p| geom::contains(lot, *p)) {
                out.buildings_outside_lot += 1;
            }
        }
    }
    let mut fills: Vec<f64> = Vec::new();
    for b in city.layout.buildings.values() {
        if let Some(lot) = lot_ring.get(&b.lot.0) {
            let a = area(lot);
            if a > 0.0 {
                fills.push(area(&geom::from_polygon(&b.footprint)) / a);
            }
        }
    }
    fills.sort_by(f64::total_cmp);
    if !fills.is_empty() {
        out.lot_fill = fills[fills.len() / 2];
    }
    let shares = stroke_shares(city);
    out.longest_stroke = shares.first().copied().unwrap_or(0.0);
    out.through_streets = shares
        .iter()
        .filter(|s| **s >= THROUGH_STREET_SHARE)
        .count();

    // Is it a coin? The blocks tile the built ground without overlapping, so the
    // sum of their areas *is* the footprint.
    let hull = geom::convex_hull(&blocks.iter().flatten().copied().collect::<Vec<Pt>>());
    let hull_area = area(&hull);
    out.solidity = if hull_area > 0.0 {
        total_block / hull_area
    } else {
        1.0
    };
    let (spokes, boulevards) = radial_convergence(city);
    out.radial_spokes = spokes;
    out.radial_strokes = boulevards;
    let (straight, rulers) = ruler_borders(city);
    out.straight_border = straight;
    out.straight_borders = rulers;
    out
}

/// Deviation from radial, in radians, of the chord `a → b` seen from `centre`.
fn radial_deviation(a: Pt, b: Pt, centre: Pt) -> f64 {
    let mid = mul(add(a, b), 0.5);
    let r = crate::geom::norm(sub(mid, centre));
    let c = crate::geom::norm(sub(b, a));
    if len(r) < 0.5 || len(c) < 0.5 {
        return std::f64::consts::FRAC_PI_2;
    }
    dot(r, c).abs().clamp(0.0, 1.0).acos()
}

/// The ruler probe: the longest dead-straight run of district border, as a
/// share of the city diameter, and how many run past
/// [`STRAIGHT_BORDER_SHARE`].
///
/// See [`Structure::straight_border`] for why this is measured separately from
/// [`radial_convergence`]: a straight border stops being *radial* the moment the
/// partition picks a different bearing, and stops being a **drawn line** only
/// when the border is no longer a chord of anything.
fn ruler_borders(city: &City) -> (f64, usize) {
    let Some((graph, diameter)) = stroke_graph(city) else {
        return (0.0, 0);
    };
    let not_border: Vec<bool> = district_border_edges(city, &graph)
        .into_iter()
        .map(|b| !b)
        .collect();
    let mut longest = 0.0f64;
    let mut over = 0usize;
    for chain in roads::strokes_excluding(&graph, RADIAL_STRAIGHT_COS, &not_border) {
        let mut ends: Vec<Pt> = Vec::with_capacity(chain.len() * 2);
        for &e in &chain {
            let (a, b) = graph.edges[e];
            ends.push(graph.nodes[a as usize]);
            ends.push(graph.nodes[b as usize]);
        }
        let mut span = 0.0f64;
        for (i, a) in ends.iter().enumerate() {
            for b in &ends[i + 1..] {
                span = span.max(dist(*a, *b));
            }
        }
        let share = span / diameter;
        longest = longest.max(share);
        if share >= STRAIGHT_BORDER_SHARE {
            over += 1;
        }
    }
    (longest, over)
}

/// The pie-chart probe: radial district borders, and boulevards through the
/// middle. See [`Structure::radial_spokes`].
fn radial_convergence(city: &City) -> (usize, usize) {
    let Some((graph, diameter)) = stroke_graph(city) else {
        return (0, 0);
    };
    let centre = city
        .layout
        .districts
        .get(&LogicalPath::root())
        .map_or([0.0, 0.0], |d| geom::from_point(d.centre));
    let radius = graph
        .nodes
        .iter()
        .map(|p| dist(*p, centre))
        .fold(1e-9f64, f64::max);

    // (a) Straight chains of district border, from the middle out to the rim.
    // Everything that is not a district border is withheld from the walk, so a
    // chain is a run of border edges and nothing else.
    let not_border: Vec<bool> = district_border_edges(city, &graph)
        .into_iter()
        .map(|b| !b)
        .collect();
    let mut spokes = 0usize;
    for chain in roads::strokes_excluding(&graph, RADIAL_STRAIGHT_COS, &not_border) {
        let mut ends: Vec<Pt> = Vec::with_capacity(chain.len() * 2);
        for &e in &chain {
            let (a, b) = graph.edges[e];
            ends.push(graph.nodes[a as usize]);
            ends.push(graph.nodes[b as usize]);
        }
        let mut span = (0.0f64, [0.0; 2], [0.0; 2]);
        for (i, a) in ends.iter().enumerate() {
            for b in &ends[i + 1..] {
                let d = dist(*a, *b);
                if d > span.0 {
                    span = (d, *a, *b);
                }
            }
        }
        if span.0 < diameter * RADIAL_MIN_SHARE {
            continue;
        }
        let lo = ends
            .iter()
            .map(|p| dist(*p, centre))
            .fold(f64::INFINITY, f64::min);
        let hi = ends.iter().map(|p| dist(*p, centre)).fold(0.0f64, f64::max);
        if radial_deviation(span.1, span.2, centre) <= RADIAL_TOLERANCE
            && lo <= radius * RADIAL_INNER
            && hi >= radius * RADIAL_OUTER
        {
            spokes += 1;
        }
    }

    // (b) Strokes that pass through the middle on opposite bearings.
    let skip = roads::perimeter_edges(&graph);
    let mut boulevards = 0usize;
    for chain in roads::strokes_excluding(&graph, STROKE_TURN_COS, &skip) {
        let mut ends: Vec<Pt> = Vec::with_capacity(chain.len() * 2);
        let mut approach = f64::INFINITY;
        for &e in &chain {
            let (a, b) = graph.edges[e];
            let (pa, pb) = (graph.nodes[a as usize], graph.nodes[b as usize]);
            approach = approach.min(crate::geom::dist_to_seg(centre, pa, pb));
            ends.push(pa);
            ends.push(pb);
        }
        if approach > radius * CIVIC_APPROACH {
            continue;
        }
        let far = ends.iter().copied().fold([0.0; 2], |best, p| {
            if dist(p, centre) > dist(best, centre) {
                p
            } else {
                best
            }
        });
        let other = ends.iter().copied().fold(far, |best, p| {
            if dist(p, far) > dist(best, far) {
                p
            } else {
                best
            }
        });
        let (u, v) = (sub(far, centre), sub(other, centre));
        if len(u) < radius * 0.3 || len(v) < radius * 0.3 {
            continue;
        }
        let cos = dot(crate::geom::norm(u), crate::geom::norm(v)).clamp(-1.0, 1.0);
        if cos <= OPPOSITE_COS {
            boulevards += 1;
        }
    }
    (spokes, boulevards)
}

/// Which graph edges separate two blocks of different districts.
///
/// Keyed on the pair of quantised endpoints, because a block ring and the graph
/// are two views of the same welded corners and the coordinates agree exactly
/// once quantised.
fn district_border_edges(city: &City, graph: &Graph) -> Vec<bool> {
    type Key = ((i64, i64), (i64, i64));
    let at = |p: Pt| -> (i64, i64) {
        (
            (crate::determinism::quantize_f64(p[0]) * 1_000.0).round() as i64,
            (crate::determinism::quantize_f64(p[1]) * 1_000.0).round() as i64,
        )
    };
    let key = |a: Pt, b: Pt| -> Key {
        let (x, y) = (at(a), at(b));
        if x <= y {
            (x, y)
        } else {
            (y, x)
        }
    };
    let mut owner: BTreeMap<Key, Vec<usize>> = BTreeMap::new();
    for (bi, b) in city.layout.blocks.iter().enumerate() {
        let ring = geom::from_polygon(&b.boundary);
        for i in 0..ring.len() {
            owner
                .entry(key(ring[i], ring[(i + 1) % ring.len()]))
                .or_default()
                .push(bi);
        }
    }
    graph
        .edges
        .iter()
        .map(|&(a, b)| {
            owner
                .get(&key(graph.nodes[a as usize], graph.nodes[b as usize]))
                .is_some_and(|list| {
                    list.len() == 2
                        && city.layout.blocks[list[0]].district
                            != city.layout.blocks[list[1]].district
                })
        })
        .collect()
}

/// Every natural road stroke's reach, as a share of the city diameter, longest
/// first.
fn stroke_shares(city: &City) -> Vec<f64> {
    let Some((graph, diameter)) = stroke_graph(city) else {
        return Vec::new();
    };
    // The city limit is a drawn boundary, not a street: see
    // [`roads::stroke_reaches`] for why leaving it in measures the outline
    // instead of the plan.
    let skip = roads::perimeter_edges(&graph);
    roads::stroke_reaches(&graph, STROKE_TURN_COS, &skip)
        .into_iter()
        .map(|r| r / diameter)
        .collect()
}

/// The longest stroke **including** the city limit, as a share of the diameter.
///
/// Printed next to [`Structure::longest_stroke`] so that excluding the outline
/// is a visible decision rather than a silent one.
#[must_use]
pub fn longest_stroke_with_limit(city: &City) -> f64 {
    let Some((graph, diameter)) = stroke_graph(city) else {
        return 0.0;
    };
    roads::stroke_reaches(&graph, STROKE_TURN_COS, &[])
        .first()
        .copied()
        .unwrap_or(0.0)
        / diameter
}

/// The published road graph, rebuilt for measurement, with the city's diameter.
fn stroke_graph(city: &City) -> Option<(Graph, f64)> {
    let nodes: Vec<Pt> = city
        .layout
        .roads
        .nodes
        .iter()
        .map(|n| geom::from_point(n.position))
        .collect();
    if nodes.len() < 2 {
        return None;
    }
    let mut graph = Graph {
        nodes,
        edges: city
            .layout
            .roads
            .segments
            .iter()
            .map(|s| (s.from.0.min(s.to.0), s.from.0.max(s.to.0)))
            .collect(),
        adj: Vec::new(),
        class: Vec::new(),
    };
    graph.rebuild_public();
    // The **diameter**, not the bounding box's diagonal. For a roughly round
    // city the diagonal is `2R√2` against a true diameter of `2R`, so every
    // stroke measured against it reads a factor of `√2` shorter than it is — the
    // difference between a 35 % avenue and a 49 % one, on exactly the number the
    // bake-off set a range for. Max pairwise over the convex hull is exact and
    // costs nothing at this size.
    let hull = geom::convex_hull(&graph.nodes);
    let mut diameter = 0.0f64;
    for (i, a) in hull.iter().enumerate() {
        for b in &hull[i + 1..] {
            diameter = diameter.max(dist(*a, *b));
        }
    }
    if diameter <= 0.0 {
        return None;
    }
    Some((graph, diameter))
}

// ---------------------------------------------------------------------------
// Serialization (PRD §16)
// ---------------------------------------------------------------------------

/// Serializes a layout for the golden-file test, quantised (PRD §16).
///
/// Pretty-printed JSON with sorted keys, so a diff between two golden files is
/// readable by a human deciding whether the change was intended.
///
/// This is the layout half only. [`City::snapshot`] is what the golden file
/// actually stores, because a layout with no terrain digest and no landmark
/// layer would pass while both moved.
pub fn snapshot(layout: &CityLayout) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&LayoutSnapshot::of(layout))
}

/// A quantised copy, ready for serialization or for a byte comparison.
///
/// Separate from [`snapshot`] so a determinism test can compare two layouts
/// structurally without going through JSON, and so the nondeterminism hunt
/// (PRD §16) can diff two in-process runs.
#[must_use]
pub fn quantized(layout: &CityLayout) -> CityLayout {
    CityLayout {
        schema: layout.schema,
        extent: quantize(layout.extent),
        roads: RoadGraph {
            nodes: layout
                .roads
                .nodes
                .iter()
                .map(|n| crate::RoadNode {
                    position: quantize_point(n.position),
                })
                .collect(),
            segments: layout.roads.segments.clone(),
            snap_tolerance: quantize(layout.roads.snap_tolerance),
        },
        blocks: layout
            .blocks
            .iter()
            .map(|b| Block {
                id: b.id,
                boundary: quantize_polygon(&b.boundary),
                district: b.district.clone(),
            })
            .collect(),
        lots: layout
            .lots
            .iter()
            .map(|l| Lot {
                id: l.id,
                block: l.block,
                boundary: quantize_polygon(&l.boundary),
                occupant: l.occupant.clone(),
            })
            .collect(),
        buildings: layout
            .buildings
            .iter()
            .map(|(path, b)| {
                (
                    path.clone(),
                    Building {
                        path: b.path.clone(),
                        lot: b.lot,
                        footprint: quantize_polygon(&b.footprint),
                        height: quantize(b.height),
                        roof: b.roof,
                        rotation: quantize(b.rotation),
                    },
                )
            })
            .collect(),
        districts: layout
            .districts
            .iter()
            .map(|(path, d)| {
                (
                    path.clone(),
                    District {
                        path: d.path.clone(),
                        boundary: quantize_polygon(&d.boundary),
                        centre: quantize_point(d.centre),
                        blocks: d.blocks.clone(),
                    },
                )
            })
            .collect(),
        streets: layout
            .streets
            .iter()
            .map(|s| StreetLine {
                from: s.from.clone(),
                to: s.to.clone(),
                edge_count: s.edge_count,
                polyline: s.polyline.iter().copied().map(quantize_point).collect(),
            })
            .collect(),
    }
}

/// [`quantize_point`] over every vertex.
#[must_use]
pub fn quantize_polygon(polygon: &Polygon) -> Polygon {
    Polygon::new(
        polygon
            .vertices
            .iter()
            .copied()
            .map(quantize_point)
            .collect(),
    )
}

/// A coordinate, printed at exactly [`SNAPSHOT_DECIMALS`] decimals.
///
/// Quantising alone is not enough for a byte comparison. `serde_json` prints an
/// `f32` through the shortest representation that round-trips, and two `f32`
/// values one ulp apart print differently — so a value that quantised to the
/// same grid point on two machines could still differ if the quantiser itself
/// saw inputs an ulp apart. Rounding the quantised value in `f64` and printing
/// *that* collapses both cases onto the same decimal string.
#[derive(Debug, Clone, Copy)]
struct Q(f32);

/// `10^SNAPSHOT_DECIMALS`, as an exactly representable `f64`.
///
/// Written as a literal rather than computed as `1 / QUANTUM`, and the
/// difference is not cosmetic: [`QUANTUM`] is an `f32`, so `1.0 /
/// f64::from(QUANTUM)` is **999.999952…**, and dividing by that reintroduces
/// exactly the long decimal tail this type exists to remove.
/// `snapshot_scale_is_exact` pins it.
const PRINT_SCALE: f64 = 1_000.0;

impl Serialize for Q {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let quantised = f64::from(quantize(self.0));
        let value = (quantised * PRINT_SCALE).round() / PRINT_SCALE;
        // `-0.0` and `0.0` compare equal and print differently; `quantize`
        // removes the first, and this guards the case where the rounding above
        // reintroduces it.
        serializer.serialize_f64(if value == 0.0 { 0.0 } else { value })
    }
}

/// A point, as `[x, y]`. Two floats and four bytes of punctuation, against
/// thirteen for `{"x":…,"y":…}`, on a structure that is mostly points.
#[derive(Debug, Clone, Copy, Serialize)]
struct P(Q, Q);

impl P {
    fn of(p: Point) -> Self {
        Self(Q(p.x), Q(p.y))
    }

    fn many(points: &[Point]) -> Vec<Self> {
        points.iter().copied().map(Self::of).collect()
    }
}

/// A `u64` as a fixed-width hex string.
///
/// JSON numbers are `f64` in most readers, which silently mangles a 64-bit
/// hash. A string cannot be mangled and diffs readably.
fn hex64(value: u64) -> String {
    format!("{value:#018x}")
}

#[derive(Debug, Serialize)]
struct LayoutSnapshot {
    format: u32,
    schema: u32,
    quantum: Q,
    extent: Q,
    roads: RoadsSnapshot,
    blocks: Vec<BlockSnapshot>,
    lots: Vec<LotSnapshot>,
    buildings: Vec<BuildingSnapshot>,
    districts: Vec<DistrictSnapshot>,
    streets: Vec<StreetSnapshot>,
}

#[derive(Debug, Serialize)]
struct RoadsSnapshot {
    snap_tolerance: Q,
    nodes: Vec<P>,
    segments: Vec<SegmentSnapshot>,
}

#[derive(Debug, Serialize)]
struct SegmentSnapshot {
    from: u32,
    to: u32,
    class: &'static str,
}

#[derive(Debug, Serialize)]
struct BlockSnapshot {
    id: u32,
    district: String,
    boundary: Vec<P>,
}

#[derive(Debug, Serialize)]
struct LotSnapshot {
    id: u32,
    block: u32,
    occupant: Option<String>,
    boundary: Vec<P>,
}

#[derive(Debug, Serialize)]
struct BuildingSnapshot {
    path: String,
    lot: u32,
    height: Q,
    roof: &'static str,
    rotation: Q,
    footprint: Vec<P>,
}

#[derive(Debug, Serialize)]
struct DistrictSnapshot {
    path: String,
    centre: P,
    blocks: Vec<u32>,
    boundary: Vec<P>,
}

#[derive(Debug, Serialize)]
struct StreetSnapshot {
    from: String,
    to: String,
    edges: u32,
    polyline: Vec<P>,
}

#[derive(Debug, Serialize)]
struct TerrainSnapshot {
    seed: String,
    extent: Q,
    octaves: u32,
    relief: Q,
    districts: usize,
    digest: String,
}

#[derive(Debug, Serialize)]
struct MonumentSnapshot {
    path: String,
    rank: u32,
    entry: Option<&'static str>,
    inbound: u32,
    top_decile: bool,
}

#[derive(Debug, Serialize)]
struct IndustrialSnapshot {
    district: String,
    host: String,
    files: u32,
    bytes: u64,
    boundary: Vec<P>,
}

#[derive(Debug, Serialize)]
struct VacancySnapshot {
    lot: u32,
    former: String,
    since_unix_ms: i64,
}

#[derive(Debug, Serialize)]
struct OverflowSnapshot {
    path: String,
    shares: Option<u32>,
}

#[derive(Debug, Serialize)]
struct CitySnapshot {
    layout: LayoutSnapshot,
    terrain: TerrainSnapshot,
    monuments: Vec<MonumentSnapshot>,
    industrial: Vec<IndustrialSnapshot>,
    vacancies: Vec<VacancySnapshot>,
    host_of: Vec<(String, String)>,
    overflow: Vec<OverflowSnapshot>,
    report: CityReport,
}

impl LayoutSnapshot {
    fn of(layout: &CityLayout) -> Self {
        Self {
            format: SNAPSHOT_FORMAT,
            schema: layout.schema,
            quantum: Q(QUANTUM),
            extent: Q(layout.extent),
            roads: RoadsSnapshot {
                snap_tolerance: Q(layout.roads.snap_tolerance),
                nodes: layout
                    .roads
                    .nodes
                    .iter()
                    .map(|n| P::of(n.position))
                    .collect(),
                segments: layout
                    .roads
                    .segments
                    .iter()
                    .map(|s| SegmentSnapshot {
                        from: s.from.0,
                        to: s.to.0,
                        class: road_class_name(s.class),
                    })
                    .collect(),
            },
            blocks: layout
                .blocks
                .iter()
                .map(|b| BlockSnapshot {
                    id: b.id.0,
                    district: b.district.as_str().to_owned(),
                    boundary: P::many(&b.boundary.vertices),
                })
                .collect(),
            lots: layout
                .lots
                .iter()
                .map(|l| LotSnapshot {
                    id: l.id.0,
                    block: l.block.0,
                    occupant: l.occupant.as_ref().map(|p| p.as_str().to_owned()),
                    boundary: P::many(&l.boundary.vertices),
                })
                .collect(),
            // `buildings` is a `BTreeMap`, so this is logical-path order.
            buildings: layout
                .buildings
                .values()
                .map(|b| BuildingSnapshot {
                    path: b.path.as_str().to_owned(),
                    lot: b.lot.0,
                    height: Q(b.height),
                    roof: roof_form_name(b.roof),
                    rotation: Q(b.rotation),
                    footprint: P::many(&b.footprint.vertices),
                })
                .collect(),
            districts: layout
                .districts
                .values()
                .map(|d| DistrictSnapshot {
                    path: d.path.as_str().to_owned(),
                    centre: P::of(d.centre),
                    blocks: d.blocks.iter().map(|b| b.0).collect(),
                    boundary: P::many(&d.boundary.vertices),
                })
                .collect(),
            streets: layout
                .streets
                .iter()
                .map(|s| StreetSnapshot {
                    from: s.from.as_str().to_owned(),
                    to: s.to.as_str().to_owned(),
                    edges: s.edge_count,
                    polyline: P::many(&s.polyline),
                })
                .collect(),
        }
    }
}

impl CitySnapshot {
    fn of(city: &City) -> Self {
        Self {
            layout: LayoutSnapshot::of(&city.layout),
            terrain: TerrainSnapshot {
                seed: hex64(city.terrain.seed),
                extent: Q(city.terrain.extent),
                octaves: city.terrain.octaves,
                relief: Q(city.terrain.relief),
                districts: city.terrain.districts,
                digest: hex64(city.terrain.digest),
            },
            monuments: city
                .monuments
                .iter()
                .map(|m| MonumentSnapshot {
                    path: m.path.as_str().to_owned(),
                    rank: m.rank,
                    entry: m.entry.map(entry_point_name),
                    inbound: m.inbound,
                    top_decile: m.top_decile,
                })
                .collect(),
            industrial: city
                .industrial
                .iter()
                .map(|i| IndustrialSnapshot {
                    district: i.district.as_str().to_owned(),
                    host: i.host.as_str().to_owned(),
                    files: i.files,
                    bytes: i.bytes,
                    boundary: P::many(&i.boundary.vertices),
                })
                .collect(),
            vacancies: city
                .vacancies
                .iter()
                .map(|v| VacancySnapshot {
                    lot: v.lot.0,
                    former: v.former.as_str().to_owned(),
                    since_unix_ms: v.since.unix_millis(),
                })
                .collect(),
            host_of: city
                .host_of
                .iter()
                .map(|(k, v)| (k.as_str().to_owned(), v.as_str().to_owned()))
                .collect(),
            overflow: city
                .overflow
                .iter()
                .map(|o| OverflowSnapshot {
                    path: o.path.as_str().to_owned(),
                    shares: o.shares.map(|l| l.0),
                })
                .collect(),
            report: city.report,
        }
    }
}

/// A stable name for a road class. Written out rather than derived, because the
/// derived name is a rename away from invalidating every golden file silently.
fn road_class_name(class: RoadClass) -> &'static str {
    match class {
        RoadClass::Arterial => "arterial",
        RoadClass::Street => "street",
        RoadClass::Alley => "alley",
    }
}

/// A stable name for a roof form. Same reasoning as [`road_class_name`].
fn roof_form_name(roof: RoofForm) -> &'static str {
    match roof {
        RoofForm::Flat => "flat",
        RoofForm::Stepped => "stepped",
        RoofForm::Pitched => "pitched",
    }
}

/// A stable name for an entry-point kind. Same reasoning as [`road_class_name`].
fn entry_point_name(entry: EntryPointKind) -> &'static str {
    match entry {
        EntryPointKind::Main => "main",
        EntryPointKind::LibraryRoot => "library-root",
        EntryPointKind::Cli => "cli",
        EntryPointKind::Server => "server",
        EntryPointKind::Router => "router",
        EntryPointKind::Index => "index",
    }
}

// ---------------------------------------------------------------------------
// Deformation rate limiting (PRD §7.7)
// ---------------------------------------------------------------------------

/// Batches layout changes so the ground never moves under the operator's eye.
///
/// > **Never move the ground while the operator is looking at it.** Batch layout
/// > changes, apply them on a slow tween (≥800ms), and prefer to defer until the
/// > camera has been still for a moment. A map that rearranges mid-read is worse
/// > than one that is slightly stale. (PRD §7.7)
///
/// # The API this exposes, and why each piece is needed
///
/// | Call | What it makes implementable |
/// |---|---|
/// | [`submit`](Self::submit) | "batch layout changes" — a burst of file writes becomes one change |
/// | [`take_pending`](Self::take_pending) | the batch, merged, as one [`RepoDelta`] |
/// | [`tween`](Self::tween) | "apply them on a slow tween (≥800 ms)", scaled by how much moved |
/// | `camera_still_for` | "prefer to defer until the camera has been still" |
/// | [`MAX_DEFERRALS`] | "prefer", not "require" — an operator who never stops panning still gets their map |
///
/// # No clock
///
/// This type never reads the clock. `camera_still_for` is supplied by the
/// caller, which is both what PRD §7.4 requires of this crate and what makes the
/// policy testable: the whole deferral ladder can be driven from a test in
/// microseconds.
#[derive(Debug, Default)]
pub struct DeformationLimiter {
    pending: RepoDelta,
    deferrals: u32,
}

impl DeformationLimiter {
    /// An empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a change and reports whether it may be applied now.
    ///
    /// `camera_still_for` is how long the camera has been stationary; a change
    /// prefers to wait for a still camera but must not wait forever, or a repo
    /// under active edit never updates its map at all.
    ///
    /// Returns `true` when the caller should take the batch and start the tween.
    /// Returning `true` does **not** clear the batch — [`take_pending`] does, so
    /// a caller that decides not to apply after all keeps its changes.
    ///
    /// [`take_pending`]: Self::take_pending
    pub fn submit(&mut self, delta: RepoDelta, camera_still_for: Duration) -> bool {
        merge_delta(&mut self.pending, delta);
        if self.pending.is_empty() {
            return false;
        }
        if camera_still_for >= Duration::from_millis(CAMERA_STILL_MS)
            || self.deferrals >= MAX_DEFERRALS
            || self.pending.len() >= MAX_DEFERRED_CHANGES
        {
            return true;
        }
        self.deferrals = self.deferrals.saturating_add(1);
        false
    }

    /// Everything queued and not yet applied, merged into one delta.
    pub fn take_pending(&mut self) -> RepoDelta {
        self.deferrals = 0;
        std::mem::take(&mut self.pending)
    }

    /// The batch as it stands, without taking it.
    #[must_use]
    pub fn pending(&self) -> &RepoDelta {
        &self.pending
    }

    /// True when nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// How many times the current batch has been deferred.
    #[must_use]
    pub fn deferrals(&self) -> u32 {
        self.deferrals
    }

    /// How long the tween for the current batch should take (PRD §7.7).
    ///
    /// At least [`MIN_TWEEN_MS`], plus [`TWEEN_MS_PER_CHANGE`] per changed file,
    /// capped at [`MAX_TWEEN_MS`]. A bigger deformation moves more slowly,
    /// because it is a bigger thing to move under someone who is reading it.
    #[must_use]
    pub fn tween(&self) -> Duration {
        let changes = u64::try_from(self.pending.len()).unwrap_or(u64::MAX);
        let extra = changes.saturating_mul(TWEEN_MS_PER_CHANGE);
        Duration::from_millis(MIN_TWEEN_MS.saturating_add(extra).min(MAX_TWEEN_MS))
    }
}

/// Merges `from` into `into`, cancelling a file that appeared and vanished.
///
/// `added` keeps its order — it is git growth order (PRD §7.1) and the later
/// batch is genuinely later — while the three set-like lists are canonicalised,
/// so the merged delta does not depend on the order changes were submitted in.
fn merge_delta(into: &mut RepoDelta, from: RepoDelta) {
    let RepoDelta {
        added,
        removed,
        resized,
        retouched,
    } = from;

    for path in removed {
        // Added and removed inside one batch: for the map, it never existed.
        if let Some(i) = into.added.iter().position(|p| *p == path) {
            into.added.remove(i);
            continue;
        }
        into.removed.push(path);
    }
    for path in added {
        if let Some(i) = into.removed.iter().position(|p| *p == path) {
            // Removed and re-added: the file is back, so it is a change to a
            // building that still exists rather than a new one.
            into.removed.remove(i);
            into.resized.push(path);
            continue;
        }
        if !into.added.contains(&path) {
            into.added.push(path);
        }
    }
    into.resized.extend(resized);
    into.retouched.extend(retouched);

    into.removed = crate::determinism::canonical_order(into.removed.drain(..));
    into.resized = crate::determinism::canonical_order(
        into.resized
            .drain(..)
            .filter(|p| !into.added.contains(p) && !into.removed.contains(p))
            .collect::<Vec<_>>(),
    );
    into.retouched = crate::determinism::canonical_order(
        into.retouched
            .drain(..)
            .filter(|p| {
                !into.added.contains(p) && !into.removed.contains(p) && !into.resized.contains(p)
            })
            .collect::<Vec<_>>(),
    );
}

#[cfg(test)]
mod tests {
    // Determinism assertions here are exact and bit-level on purpose (PRD §7.4,
    // §16); `float_cmp` exists to catch approximate equality written as `==`,
    // which is the opposite of what these tests are for.
    #![allow(clippy::float_cmp)]

    use super::*;
    use polis_repo::synthetic;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("a valid test path")
    }

    fn town(files: usize) -> City {
        generate_city(&synthetic::repository(files, 0x0D15_EA5E_0000_0001))
    }

    /// The growth step's caches are a clock optimisation and nothing else.
    ///
    /// Two cities are grown by the same six files. One keeps everything the
    /// previous step worked out; the other is made to forget it before every
    /// step, so every block is re-cut and every building re-seated from nothing.
    /// The two must be **byte-identical** — PRD §7.4 does not have an exception
    /// for "it was faster the second way".
    ///
    /// This is the test that makes `crate::memo` safe to extend: a key that
    /// stops covering one of the seating function's inputs fails here, on the
    /// first run, rather than as a building that quietly kept a stale shape.
    #[test]
    fn a_warm_cache_and_a_cold_one_build_the_same_city() {
        let mut tree = synthetic::repository(600, 0x0D15_EA5E_0000_0001);
        let inputs = LayoutInputs::default();
        let mut warm = generate_with(&tree, &inputs);
        let mut cold = generate_with(&tree, &inputs);
        assert_eq!(warm.digest(), cold.digest(), "the two starts differ");

        // Reuse is read after **every** step, not once at the end. `reuse()`
        // reports the assembly that just ran, and the six steps are not alike:
        // a file that joins a plot already on the ground changes no cell at all,
        // while a file that founds a new plot re-cuts that plot's neighbourhood.
        // Reading only the last one measures whichever kind step five happened
        // to be — which is a coin toss, not a property of the caches.
        let mut cut_rates = Vec::new();
        let mut seat_rates = Vec::new();
        for i in 0..6u32 {
            let path = lp(&format!("core/newcomer{i}.rs"));
            let mut meta = polis_repo::FileMeta::untracked(path.clone(), 2_500 + u64::from(i) * 91);
            meta.growth_index = u32::try_from(tree.files.len()).expect("fits");
            tree.files.insert(path.clone(), meta);

            warm.accrete(&tree, &inputs, std::slice::from_ref(&path));
            cold.forget();
            cold.accrete(&tree, &inputs, std::slice::from_ref(&path));
            assert_eq!(
                warm.digest(),
                cold.digest(),
                "add {i}: the remembered city and the recomputed one diverged"
            );
            let (c, s) = warm.reuse();
            cut_rates.push(c);
            seat_rates.push(s);
        }
        // And the caches were actually doing something, or the test proves
        // nothing at all.
        //
        // Measured on this fixture: three steps join an existing plot and reuse
        // 99–100 % of buildings; three found one and reuse 34–38 %, because a
        // new Voronoi site moves its neighbours' boundaries and a moved block
        // ring is a different key. The mean over the run is 56 % of cuts and
        // 68 % of buildings. At the scale the budget is actually written
        // against, `tests/incremental_budget.rs` measures 62 % / 76 % at 1 000
        // files with seven of 1 003 nodes moved — the churn is a fixed
        // neighbourhood, so its *share* falls as the city grows.
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
        let (cuts, seats) = (mean(&cut_rates), mean(&seat_rates));
        assert!(
            cuts > 0.5 && seats > 0.5,
            "over six growth steps the city reused {:.0}% of block cuts and {:.0}% of buildings ({:?} / {:?}); the caches are not being used and this test is vacuous",
            cuts * 100.0,
            seats * 100.0,
            cut_rates.iter().map(|r| (r * 100.0) as u32).collect::<Vec<_>>(),
            seat_rates.iter().map(|r| (r * 100.0) as u32).collect::<Vec<_>>(),
        );
        // A step that only fills an existing plot must reuse essentially
        // everything: that is the case the cache exists for, and a key that
        // stopped covering one of the seating function's inputs would miss here
        // even though the geometry did not move.
        let quiet = seat_rates.iter().copied().fold(0.0f64, f64::max);
        assert!(
            quiet > 0.95,
            "the best of six growth steps still re-seated {:.0}% of the city's buildings",
            (1.0 - quiet) * 100.0
        );
        assert_eq!(cold.reuse().1, 0.0, "the cold city reused a building");
    }

    /// Euler's formula, on a real generated city.
    ///
    /// The face walk is the one piece where a subtle bug is invisible until the
    /// block count is wrong, and `faces == E − V + C` is the cheap invariant
    /// that catches it. It is asserted here and again on a fixed corpus in the
    /// M1 gate.
    #[test]
    fn blocks_equal_e_minus_v_plus_c() {
        for n in [120, 400, 900] {
            let city = town(n);
            let r = city.report;
            assert_eq!(
                r.blocks,
                (r.road_segments + r.components).saturating_sub(r.road_nodes),
                "Euler's formula broke at {n} files: {r:?}"
            );
            assert_eq!(r.cycles, r.blocks, "cycles and bounded faces disagree");
        }
    }

    #[test]
    fn the_road_network_is_one_planar_connected_thing() {
        let city = town(600);
        let r = city.report;
        assert_eq!(r.components, 1, "the city is in {} pieces", r.components);
        assert_eq!(r.dangling, 0, "a road ends in mid-air");
        assert_eq!(r.crossings, 0, "two roads cross without a junction");
        assert!(r.cycles > 0, "the road graph is a tree: {r:?}");
        assert!(
            r.complex_junctions * 3 > r.junctions,
            "only {} of {} junctions are four-way or better; PRD §7.2's organic \
             signature is missing",
            r.complex_junctions,
            r.junctions
        );
    }

    #[test]
    fn no_building_stands_in_a_road_or_outside_its_lot() {
        let city = town(700);
        let s = measure(&city);
        assert_eq!(s.buildings_on_road, 0, "a building is in the road corridor");
        assert_eq!(s.buildings_outside_lot, 0, "a building left its own lot");
    }

    #[test]
    fn every_file_that_should_have_a_building_has_one() {
        let tree = synthetic::repository(500, 7);
        let city = generate_city(&tree);
        let housed = tree.files.values().filter(|f| !f.class.is_massed()).count();
        assert_eq!(
            city.report.buildings + city.report.unbuilt + city.report.overflow,
            housed,
            "files went missing between the tree and the map: {:?}",
            city.report
        );
        assert!(
            city.report.unbuilt * 200 < housed,
            "{} of {housed} files got no building",
            city.report.unbuilt
        );
    }

    #[test]
    fn the_ground_is_built_on() {
        // The design bake-off's cross-cutting finding: at 8.9 % coverage a plan
        // reads as a diagram. A dense core runs 30–60 %.
        let city = town(900);
        let s = measure(&city);
        assert!(
            s.coverage > 0.22,
            "only {:.1}% of the ground is built on",
            s.coverage * 100.0
        );
        assert!(
            s.coverage < 0.70,
            "{:.1}% coverage leaves no streets",
            s.coverage * 100.0
        );
    }

    #[test]
    fn the_old_town_has_a_finer_grain_than_the_rim() {
        let city = town(900);
        let s = measure(&city);
        assert!(
            s.age_gradient > 1.4,
            "the age gradient is {:.2}x; PRD §7.1's old town is not visible",
            s.age_gradient
        );
    }

    #[test]
    fn there_are_through_streets() {
        // The bake-off's second defect: the longest natural stroke was 28 % of
        // the city diameter, which is the soap-foam tell.
        let city = town(900);
        let s = measure(&city);
        assert!(
            s.longest_stroke > 0.35,
            "the longest road stroke is {:.0}% of the city diameter",
            s.longest_stroke * 100.0
        );
    }

    #[test]
    fn two_runs_in_one_process_are_byte_identical() {
        let tree = synthetic::repository(300, 11);
        let first = generate_city(&tree);
        let second = generate_city(&tree);
        assert_eq!(first.digest(), second.digest());
        assert_eq!(
            first.snapshot().expect("serializes"),
            second.snapshot().expect("serializes")
        );
    }

    #[test]
    fn caller_input_order_cannot_move_the_city() {
        let tree = synthetic::repository(300, 13);
        let inputs = LayoutInputs {
            streets: vec![
                polis_repo::imports::Street {
                    from: lp("core"),
                    to: lp("web"),
                    edge_count: 4,
                },
                polis_repo::imports::Street {
                    from: lp("web"),
                    to: lp("core"),
                    edge_count: 2,
                },
            ],
            inbound: vec![(lp("core/index0.rs"), 9), (lp("web/index0.rs"), 3)],
            diff_lines: BTreeMap::new(),
        };
        let reference = generate_with(&tree, &inputs)
            .snapshot()
            .expect("serializes");
        let mut permuted = inputs.clone();
        permuted.streets.reverse();
        permuted.inbound.reverse();
        assert!(
            generate_with(&tree, &permuted)
                .snapshot()
                .expect("serializes")
                == reference,
            "the city moved when `LayoutInputs` was permuted (PRD §7.4)"
        );
        // Unconditionally, in every profile: the emitted order is the layout's.
        let emitted: Vec<(LogicalPath, LogicalPath)> = generate_with(&tree, &permuted)
            .layout
            .streets
            .iter()
            .map(|s| (s.from.clone(), s.to.clone()))
            .collect();
        let mut sorted = emitted.clone();
        sorted.sort();
        assert_eq!(emitted, sorted);
    }

    #[test]
    fn a_district_is_one_place_on_the_map() {
        let city = town(900);
        let r = city.report;
        // Zero, not "few": `districts` rules T and A make a district's blocks
        // one edge-connected region by construction, so any fragment at all is
        // a real failure of the construction rather than a tuning miss.
        assert_eq!(
            r.fragmented_districts, 0,
            "{} of {} districts are in more than one piece",
            r.fragmented_districts, r.districts
        );
        assert_eq!(
            r.fragmented_packages, 0,
            "{} packages are in more than one piece",
            r.fragmented_packages
        );
        // A **rate**, not a zero, and the difference is the point — but the
        // difference is not the one it used to be, and it is worth being exact
        // about what changed.
        //
        // The two properties above are absolute, and they are now guaranteed by
        // `regions`: a district's plots are one connected part of the plot
        // adjacency graph by construction, so any fragment at all is a failure
        // of that construction. The growth's preference for budding a plot onto
        // its own district's ground is no longer the mechanism that delivers
        // them — it is a *shaping weight*, and what it buys is that the graph
        // partition starts from blobs rather than from confetti and therefore
        // moves few plots.
        //
        // So the bend rate is a quality number, not a correctness one, and the
        // bound is set where a real regression would show. Measured: 26 of 337
        // plots here, 123 of 955 at 5 000 files. A third is twice the worst of
        // those, and a growth that had stopped preferring its own ground at all
        // would sit near the share of the frontier that is foreign — well past
        // it.
        assert!(
            r.settled_nonadjacent * 3 <= r.plots,
            "{} of {} plots were founded out of contact with their own district",
            r.settled_nonadjacent,
            r.plots
        );
        assert_eq!(r.detached_placements, 0);
        assert_eq!(r.unhoused, 0, "a file's plot fell outside every block");
    }

    #[test]
    fn one_more_file_is_one_more_growth_step() {
        let mut tree = synthetic::repository(400, 17);
        let mut city = generate_city(&tree);
        let before = city.report.buildings + city.report.unbuilt;
        let path = lp("core/newcomer.rs");
        let mut meta = polis_repo::FileMeta::untracked(path.clone(), 4_096);
        meta.growth_index = u32::try_from(tree.files.len()).expect("fits");
        tree.files.insert(path.clone(), meta);
        let moved = city.accrete(&tree, &LayoutInputs::default(), std::slice::from_ref(&path));
        assert!(
            city.building(&path).is_some(),
            "the new file got no building"
        );
        // One more file is one more *housed* file. Counted as `buildings +
        // unbuilt` — files with a parcel of their own — rather than as buildings
        // alone, because a growth step also reshapes the parcels around the new
        // plot, and a marginal parcel that gains or loses its road-clear
        // interior in the process is a fact about `lots`/`buildings`, measured
        // by `report.unbuilt`, not about whether growth is incremental.
        assert_eq!(city.report.buildings + city.report.unbuilt, before + 1);
        assert_eq!(city.report.components, 1, "the growth step split the city");
        assert!(
            moved < city.report.road_nodes,
            "a single add moved every road node"
        );
    }

    #[test]
    fn a_snapshot_round_trips_through_the_quantiser() {
        let city = town(200);
        let once = quantized(&city.layout);
        let twice = quantized(&once);
        assert_eq!(
            snapshot(&once).expect("serializes"),
            snapshot(&twice).expect("serializes"),
            "quantisation is not idempotent"
        );
    }

    #[test]
    fn the_deformation_limiter_batches_and_tweens() {
        let mut limiter = DeformationLimiter::new();
        let delta = RepoDelta {
            added: vec![lp("a.rs")],
            ..RepoDelta::default()
        };
        assert!(!limiter.submit(delta.clone(), Duration::from_millis(0)));
        assert!(limiter.deferrals() > 0);
        assert!(limiter.submit(delta, Duration::from_millis(CAMERA_STILL_MS)));
        assert!(limiter.tween() >= Duration::from_millis(MIN_TWEEN_MS));
        let batch = limiter.take_pending();
        assert_eq!(batch.added.len(), 1);
        assert!(limiter.is_empty());
    }

    #[test]
    fn a_removed_file_leaves_a_vacant_lot_not_a_hole() {
        let tree = synthetic::repository(200, 23);
        let mut city = generate_city(&tree);
        let gone = city
            .layout
            .buildings
            .keys()
            .next()
            .cloned()
            .expect("a building");
        let lots_before = city.layout.lots.len();
        let delta = RepoDelta {
            removed: vec![gone.clone()],
            ..RepoDelta::default()
        };
        let outcome = city.grow(
            &tree,
            &delta,
            &LayoutInputs::default(),
            WallTime::from_unix_seconds(1_700_000_000),
        );
        assert_eq!(outcome.vacated, 1);
        assert_eq!(city.layout.lots.len(), lots_before, "a lot disappeared");
        assert!(city.building(&gone).is_none());
        assert_eq!(city.vacancies.len(), 1);
    }

    #[test]
    fn an_empty_repository_produces_an_empty_city() {
        let tree = RepoTree::default();
        let city = generate_city(&tree);
        assert!(city.layout.blocks.is_empty());
        assert!(city.layout.buildings.is_empty());
        assert!(city.snapshot().is_ok());
    }
}
