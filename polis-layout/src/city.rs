//! The pipeline, the growth step, and the serialized [`CityLayout`] (PRD §7,
//! §7.7, §16).
//!
//! This module runs the five steps in the one order they are allowed to run in
//! — terrain, roads, blocks, lots, buildings — and owns the two things nothing
//! else can own: the [`CityLayout`] that comes out, and the serialization that
//! PRD §16's golden-file test compares.
//!
//! > **Layout runs off the render thread**, incrementally, never inside a frame.
//! > (PRD §13)
//!
//! # The golden-file test is the most important test in the suite
//!
//! > Fixture repos with pinned git history; snapshot the serialized
//! > `CityLayout`. Run on two OSes in CI. This is the most important test in the
//! > suite — everything else in the product depends on the map not moving.
//!
//! Four obligations follow, and they belong here rather than in the test:
//!
//! 1. **Quantise on the way out.** [`snapshot`] applies
//!    [`crate::determinism::quantize`] to every coordinate before serializing,
//!    so a last-bit `f32` difference between two targets cannot fail a
//!    comparison no human could act on (ADR-0029). Never quantise *inside* the
//!    algorithm; that would make the layout depend on rounding at every step.
//! 2. **Print at a fixed precision.** Quantising is not enough on its own: two
//!    `f32` values one ulp apart can straddle a grid midpoint and round to two
//!    different grid points. Every float in a snapshot is therefore printed as a
//!    **three-decimal** number — [`crate::determinism::QUANTUM`] is `0.001`, so
//!    three decimals is exactly the grid and never more — which makes the byte
//!    comparison a comparison of the values a human could act on.
//! 3. **Pin the ordering of every collection.** Points are arrays, not objects;
//!    keyed collections are emitted in `BTreeMap` order; index-keyed ones in
//!    index order. There is no `HashMap` anywhere in this crate, which
//!    `no_hashmap_reaches_the_layout` asserts structurally rather than by
//!    inspection.
//! 4. **Version the format.** [`SNAPSHOT_FORMAT`] sits beside
//!    [`crate::LAYOUT_SCHEMA`] in every snapshot, because a golden file with no
//!    version is indistinguishable from a stale one.
//!
//! # The clock never reaches the layout
//!
//! Everything in [`CityLayout`] is a function of the repository, and of nothing
//! else. That is what makes the golden file a *constant*. Two things that look
//! like they belong in the layout and do not:
//!
//! * **PRD §7.5's decay.** Overgrowth and how far a vacant lot has gone to seed
//!   are functions of *now*. They are computed at render time from
//!   [`crate::buildings::overgrowth`] and [`crate::lots::Vacancy`], never
//!   stored. What *is* stored is the vacancy record — the lot, the file that
//!   used to be there, and the commit time it went — which is history, not a
//!   clock reading.
//! * **PRD §17 Q4's slow-decay ghost.** Same reason; see
//!   [`crate::buildings`]'s module docs for the shape that was chosen and why it
//!   is applied by the renderer rather than baked into `Building::height`.
//!
//! # Incremental growth, and what is honestly true about it
//!
//! > Growth is genuinely incremental — a new file runs one growth step, it does
//! > not regenerate the world. (PRD §7.4)
//!
//! [`step`] does exactly that: a new file **settles onto a free lot in its
//! district** by [`crate::lots::VacancyLedger::settle`], which is documented to
//! reproduce the answer a full [`crate::lots::assign`] over the same lot set
//! would have given it. So within one road network, incremental placement and a
//! full re-plan agree, and a deletion leaves a vacant lot rather than
//! reshuffling every neighbour.
//!
//! What is **not** true, and must not be claimed: that a city grown
//! incrementally is byte-identical to one regenerated over the extended tree.
//! [`crate::roads::scatter_attractors`] normalises each district's disc radius
//! by that district's *file count*, so adding one file to a district moves every
//! attractor in it, and therefore the roads, blocks and lots. Incrementality is
//! prefix-stable in the road-growth loop but the attractor scatter feeding it is
//! not prefix-stable under insertion. [`GrowthOutcome::rebuild_required`] is how
//! [`step`] says "this one needs a regeneration", and PRD §7.7's
//! [`DeformationLimiter`] is what keeps that regeneration from happening under
//! the operator's eye.
//!
//! This does not weaken PRD §15's M1 gate, which is about the same repository
//! state producing the same city twice, on two machines — that holds exactly.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use polis_events::{LogicalPath, WallTime};
use polis_repo::tree::EntryPointKind;
use polis_repo::{FileClass, RepoDelta, RepoTree};
use serde::{Serialize, Serializer};

use crate::blocks::{self, district_of, DistrictSites};
use crate::buildings::{self, BuildingSpec};
use crate::determinism::{fnv1a64, quantize, quantize_point, QUANTUM};
use crate::lots::{self, Overflow, VacancyLedger};
use crate::roads::{self, GrowthParams};
use crate::terrain::TerrainField;
use crate::{
    Block, BlockId, Building, CityLayout, District, Lot, LotId, Point, Polygon, RoadClass,
    RoadGraph, RoofForm, StreetLine, LAYOUT_SCHEMA,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Version of the serialized snapshot envelope (PRD §16).
///
/// Distinct from [`crate::LAYOUT_SCHEMA`], which versions the in-memory
/// [`CityLayout`]: the snapshot carries strictly more than the layout does —
/// terrain parameters, monuments, industrial masses, the vacancy ledger — and
/// its shape can change without the layout's changing. Both appear in every
/// snapshot. Bump this on any change to what is written or to the order it is
/// written in; a bump invalidates every golden file **on purpose**, which is the
/// signal, not a nuisance (ADR-0029).
pub const SNAPSHOT_FORMAT: u32 = 1;

/// Decimal places every float is printed with in a snapshot.
///
/// Exactly matches [`QUANTUM`] — `0.001` is three decimals — so the printed form
/// is the quantised value and nothing more. See the module docs, obligation 2.
pub const SNAPSHOT_DECIMALS: u32 = 3;

/// The seed [`TerrainField::generate`] is given.
///
/// **A constant, deliberately.** The obvious alternatives are all wrong for PRD
/// §15's gate:
///
/// * The repository's root *path* is machine-specific — `/home/x/repo` and
///   `C:\src\repo` are the same repository — so the same repo would grow two
///   different cities on two machines, which is precisely what M1 forbids.
/// * `HEAD` changes on every commit, which would reshape the landscape, and
///   therefore every road, every day.
/// * A hash of the file set changes whenever a file is added, which is the same
///   failure one step removed.
/// * The oldest surviving file's path is stable *until that file is deleted*,
///   at which point the whole city moves. A landmine is worse than a constant.
///
/// The terrain is never rendered (PRD §7.2); its only job is to give roads
/// contours to follow. Two repositories sharing a landscape costs nothing,
/// because what makes their cities different is the attractor scatter, which is
/// derived from the directory tree and git growth order. A caller that genuinely
/// wants a per-repository landscape passes its own seed to
/// [`generate_with_seed`] — and owns the obligation to make it machine-stable.
pub const TERRAIN_SEED: u64 = 0x91d5_1b0b_7c25_dbd8;

/// The road-growth tunables the product uses (PRD §7.2 step 2).
///
/// **Not [`GrowthParams::default`].** The defaults in [`crate::roads`] are the
/// algorithm's defaults — the values its own tests exercise it at. These are the
/// values a *city* is grown at, and they were chosen by rendering one and
/// looking at it, which is the only way this particular decision can be made.
///
/// What the render showed, and what each change is for. Measured on this
/// workspace — 86 files, a dozen districts — before and after:
///
/// | | blocks | cycles | 4-way junctions | block area / city area |
/// |---|---:|---:|---:|---:|
/// | `GrowthParams::default()` | 88 | 89 | 57 | 9.5 % |
/// | `CITY_GROWTH` | 160 | 204 | 120 | 14.8 % |
///
/// * **`branches` 2 → 3.** The largest single effect, and the cheapest. Cycles
///   are what make blocks; more branches per attractor means more of them
///   converge on it and snap together, which is PRD §7.2's organic signature —
///   "those irregular four- and five-way junctions are what the eye reads as
///   grown". Three rather than four because at four every attractor starts to
///   grow a wheel-spoke rosette that reads as a roundabout.
/// * **`segment_length` 10 → 7, `kill_radius` 7 → 5.** At the defaults the city
///   was a constellation of hamlets joined by long empty roads. Shortening both
///   tightens everything the road network builds without fragmenting it —
///   pushed further (5 and 3.5) the network fragments instead, and a 26-file
///   fixture went from housing every file to eight of them sharing lots.
/// * **`influence_radius` 25 → 18.** At 25 an attractor pulled branches out of
///   nodes most of a district away, which lengthened exactly the empty
///   connecting strands that were the problem. It wants to be about one and a
///   half attractor spacings, so a file's roads come from its own
///   neighbourhood.
///
/// `snap_radius` is deliberately left at [`crate::DEFAULT_SNAP_TOLERANCE`].
/// `GrowthParams::is_sane` requires `segment_length > 2 · snap_radius`, so
/// shortening the segment further would mean shrinking the snap radius too —
/// and snapping *is* the organic signature. Trading it for density would be
/// trading the look for the thing the look is made of.
///
/// **These numbers do not fix the layout's main visual problem**, which is that
/// district discs never touch: `roads::scatter_attractors` places them on a
/// spiral whose radius grows with cumulative area, so at any repository size
/// the city is a set of islands with 85 % empty ground between them. That is a
/// change to the scatter, not to these tunables.
pub const CITY_GROWTH: GrowthParams = GrowthParams {
    kill_radius: 5.0,
    snap_radius: crate::DEFAULT_SNAP_TOLERANCE,
    segment_length: 7.0,
    slope_threshold: roads::DEFAULT_SLOPE_THRESHOLD,
    max_steps: roads::DEFAULT_MAX_STEPS,
    branches: 3,
    influence_radius: 18.0,
};

/// Sampling resolution of the terrain digest recorded in a snapshot.
///
/// The field is never serialized — it has no geometry — so a digest is the only
/// way a golden file can notice that the landscape moved. 16 × 16 samples is
/// 256 evaluations, far below anything measurable next to road growth.
pub const TERRAIN_DIGEST_RESOLUTION: u32 = 16;

/// Most files promoted to [`FileClass::Monument`] by the layout (PRD §8).
///
/// `polis_repo::tree::monuments` ranks *candidates*: every named entry point
/// plus the top decile of inbound imports. In a workspace of eight crates that
/// is a few dozen, and PRD §8's monuments are "always labelled at every zoom",
/// where there is room for perhaps a dozen labels. The ranking is the product,
/// so the cap is applied to the ranking rather than to the criteria.
pub const MAX_MONUMENTS: usize = 24;

/// Minimum tween duration for an applied layout change (PRD §7.7).
pub const MIN_TWEEN_MS: u64 = 800;

/// Longest tween a batch may ask for, however large it is (PRD §7.7).
pub const MAX_TWEEN_MS: u64 = 4_000;

/// Extra tween milliseconds per changed file.
pub const TWEEN_MS_PER_CHANGE: u64 = 20;

/// How still the camera must be before a deferred change is applied (PRD §7.7).
///
/// > prefer to defer until the camera has been still for a moment
pub const CAMERA_STILL_MS: u64 = 250;

/// Submissions a batch may be deferred for before it is applied regardless.
///
/// > A map that rearranges mid-read is worse than one that is slightly stale.
///
/// Slightly stale, not permanently stale: an operator who never stops panning
/// must still get their map eventually, so deferral is bounded by a count rather
/// than by a clock — a clock in this crate is what PRD §7.4 forbids, and a count
/// is what a test can drive.
pub const MAX_DEFERRALS: u32 = 32;

/// Changed files a batch may accumulate before it is applied regardless.
pub const MAX_DEFERRED_CHANGES: usize = 64;

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// Everything the layout needs that is not in the [`RepoTree`].
///
/// All three are *optional* — [`Default`] produces a city with no streets, no
/// import-derived monuments and a clean working tree — because PRD §15's M1 gate
/// runs before any of them exist, and a pipeline that cannot run without an
/// import graph cannot be golden-file tested before PRD §9 lands.
#[derive(Debug, Clone, Default)]
pub struct LayoutInputs {
    /// Cross-district import relations (PRD §9), from
    /// `polis_repo::imports::ImportGraph::cross_district_edges`.
    pub streets: Vec<polis_repo::imports::Street>,
    /// Inbound import counts per file, from
    /// `polis_repo::imports::ImportGraph::inbound_counts`. Feeds PRD §8's "top
    /// decile of inbound imports" monument criterion.
    pub inbound: Vec<(LogicalPath, u32)>,
    /// Uncommitted diff lines per file — PRD §7.3's building height, taken as an
    /// **input** rather than derived by re-diffing the working tree (ADR-0042).
    pub diff_lines: BTreeMap<LogicalPath, u32>,
}

impl LayoutInputs {
    /// Diff lines for one file, zero when unknown.
    #[must_use]
    pub fn diff_lines_of(&self, path: &LogicalPath) -> u32 {
        self.diff_lines.get(path).copied().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// The aggregate
// ---------------------------------------------------------------------------

/// The terrain field's parameters, as a serializable record (PRD §7.2 step 1).
///
/// The field itself has no geometry and is never rendered, so it cannot appear
/// in [`CityLayout`] — but it decides every road contour in the city, so a
/// golden file that did not pin it would pass while the landscape moved
/// underneath. [`TerrainParams::digest`] is that pin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerrainParams {
    /// The seed the field was generated from.
    pub seed: u64,
    /// Half-width of the square the city occupies, in city-space units.
    pub extent: f32,
    /// Octaves of fBm.
    pub octaves: u32,
    /// Peak relief, in city-space units.
    pub relief: f32,
    /// Districts whose depth bias was applied.
    pub districts: usize,
    /// `TerrainField::digest(TERRAIN_DIGEST_RESOLUTION)`.
    pub digest: u64,
}

impl TerrainParams {
    /// Reads the parameters off a generated field.
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

/// A file PRD §8 labels at every zoom, placed (PRD §8).
///
/// Carries the *reason* as well as the file: an operator asking "why is that a
/// landmark" gets an answer in the drill-down, and a maintainer changing the
/// ranking can see in a golden-file diff which criterion moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonumentMark {
    /// The file.
    pub path: LogicalPath,
    /// Position in the ranking, `0` strongest. Label budget is spent in this
    /// order when zoomed out.
    pub rank: u32,
    /// Which named entry point it is, if it is one.
    pub entry: Option<EntryPointKind>,
    /// Internal imports pointing at it.
    pub inbound: u32,
    /// Whether it is in the top decile of inbound imports.
    pub top_decile: bool,
}

/// An industrial tree, drawn as one dull shape (PRD §8).
///
/// > Large, uniform, deliberately dull. Making these boring is the feature — the
/// > eye should slide off them. **Rendered as a single mass, not individual
/// > buildings.**
///
/// So there is one of these per industrial district and *no* [`Building`] for
/// any file inside it. [`crate::lots::plan`] refuses those files a lot and
/// [`crate::buildings::place_with`] refuses them a building; this is where the
/// mass they are drawn as instead is described.
#[derive(Debug, Clone, PartialEq)]
pub struct IndustrialMass {
    /// The industrial directory — `node_modules`, `target`, a vendored tree.
    pub district: LogicalPath,
    /// The district whose blocks it is drawn over, which is an ancestor when the
    /// industrial tree has no blocks of its own. Empty when the city has none.
    pub host: LogicalPath,
    /// Files inside it. The mass is sized from this, not drawn from it.
    pub files: u32,
    /// Total bytes inside it.
    pub bytes: u64,
    /// The shape to draw. The convex hull of the host district's blocks, or
    /// empty when the district has no ground at all.
    pub boundary: Polygon,
}

/// What [`generate_with`] produced, in numbers.
///
/// Every field is an assertion target. `cycles == 0` in particular is the
/// silent product failure PRD §7.2 warns about — "without snapping you get a
/// tree, and trees read as artificial" — and it compiles, runs and renders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct CityReport {
    /// Files in the tree.
    pub files: usize,
    /// Buildings placed.
    pub buildings: usize,
    /// Blocks extracted from the road graph.
    pub blocks: usize,
    /// Lots subdivided out of them.
    pub lots: usize,
    /// Lots with no occupant. Undeveloped ground plus PRD §7.5 vacancies.
    pub empty_lots: usize,
    /// Files drawn as part of an industrial mass rather than as buildings.
    pub massed: usize,
    /// Files sharing another file's lot because their district ran out.
    pub overflow: usize,
    /// Files with a lot but no building — a parcel too small for a setback.
    pub unbuilt: usize,
    /// Road nodes.
    pub road_nodes: usize,
    /// Road segments.
    pub road_segments: usize,
    /// Independent cycles in the road graph. **Zero means a tree.**
    pub cycles: usize,
    /// Nodes where three or more roads meet.
    pub junctions: usize,
    /// Nodes where four or more roads meet — PRD §7.2's organic signature.
    pub complex_junctions: usize,
    /// Monuments marked (PRD §8).
    pub monuments: usize,
}

/// The generated city: [`CityLayout`] plus everything PRD §8 needs that does not
/// fit in it.
///
/// `CityLayout` is the cross-crate contract in `lib.rs` and is deliberately not
/// extended here; the landmark layer, the terrain pin and the PRD §7.5 vacancy
/// ledger live alongside it instead. [`City::snapshot`] serializes the whole
/// aggregate, which is what PRD §16's golden file compares.
#[derive(Debug, Clone)]
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
    /// Which district each district's files were routed to, in district order.
    /// A district mapping to itself is the ordinary case.
    pub host_of: BTreeMap<LogicalPath, LogicalPath>,
    /// Files that had to share a lot. Never dropped; the renderer must draw
    /// them (PRD §7.2 step 4, `lots::plan`'s overflow policy).
    pub overflow: Vec<Overflow>,
    /// What happened, in numbers.
    pub report: CityReport,
}

impl Default for City {
    fn default() -> Self {
        Self {
            layout: CityLayout::default(),
            terrain: TerrainParams::of(&TerrainField::default()),
            monuments: Vec::new(),
            industrial: Vec::new(),
            vacancies: VacancyLedger::new(),
            host_of: BTreeMap::new(),
            overflow: Vec::new(),
            report: CityReport::default(),
        }
    }
}

impl City {
    /// The serialized form PRD §16's golden file compares, quantised and printed
    /// at [`SNAPSHOT_DECIMALS`].
    pub fn snapshot(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&CitySnapshot::of(self))
    }

    /// A stable 64-bit digest of [`City::snapshot`].
    ///
    /// What the separate-process determinism test compares: a child process can
    /// print sixteen hex digits on stdout, where it cannot hand back a
    /// megabyte of JSON without the comparison becoming a test of the pipe.
    ///
    /// # Panics
    ///
    /// Never in practice — the snapshot types contain no map with non-string
    /// keys and no non-finite float (quantisation removes those), so
    /// serialization cannot fail. A failure would mean the snapshot shape itself
    /// is broken, which is exactly when a determinism test should stop.
    #[must_use]
    pub fn digest(&self) -> u64 {
        let json = self.snapshot().expect("a snapshot always serializes");
        fnv1a64(json.as_bytes())
    }

    /// The building for a file.
    #[must_use]
    pub fn building(&self, path: &LogicalPath) -> Option<&Building> {
        self.layout.building(path)
    }

    /// True when the file is one of PRD §8's monuments.
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
}

// ---------------------------------------------------------------------------
// Generation (PRD §7.2)
// ---------------------------------------------------------------------------

/// Generates a city from scratch by replaying the growth sequence.
///
/// PRD §13.1 budgets cold start → first frame under 3 s for a 5 000-file repo;
/// PRD §16 requires this to be byte-identical across two runs and two operating
/// systems before any live data is wired in (milestone M1).
///
/// Attractors are supplied in **git growth order** (PRD §7.1) — that ordering is
/// the mechanism behind the old town, not a detail of the loop.
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
/// §15's M1 gate fails for a reason that has nothing to do with this crate. See
/// [`TERRAIN_SEED`] for why the default is a constant.
#[must_use]
#[allow(clippy::too_many_lines)] // the pipeline is one ordered sequence; splitting it hides the order
pub fn generate_with_seed(tree: &RepoTree, inputs: &LayoutInputs, seed: u64) -> City {
    let extent = roads::suggested_extent(tree);

    // --- 1. Terrain (PRD §7.2 step 1) -------------------------------------
    // The scatter has to exist before the field can be biased, because a
    // district's bias is placed at the district's centre and that centre is
    // derived from where its files landed. The field is still built first so its
    // seed and extent are fixed before anything reads it.
    let mut terrain = TerrainField::generate(seed, extent);
    let attractors = roads::scatter_attractors(tree, extent);
    let sites = DistrictSites::from_scatter(tree, &attractors);
    for district in sites.districts() {
        if let Some(centre) = sites.centre(district) {
            terrain.bias_district(district, centre);
        }
    }

    // --- 2. Roads (PRD §7.2 step 2) ---------------------------------------
    let mut roads_graph = roads::grow(&terrain, &attractors, CITY_GROWTH, RoadClass::Street);
    classify_roads(&mut roads_graph, &sites);
    let junctions = roads::junction_stats(&roads_graph);

    // --- 3. Blocks (PRD §7.2 step 3) --------------------------------------
    let mut block_list = blocks::extract(&roads_graph);
    blocks::assign_districts(&mut block_list, &sites);

    // --- 4. Lots (PRD §7.2 step 4) ----------------------------------------
    let plan = lots::plan(&block_list, tree);

    // --- 5. Buildings (PRD §7.2 step 5, §7.3, §8) -------------------------
    let monument_set = rank_monuments(tree, &inputs.inbound);
    let block_area: BTreeMap<BlockId, f32> = block_list
        .iter()
        .map(|block| (block.id, block.boundary.area()))
        .collect();
    let mut buildings_map: BTreeMap<LogicalPath, Building> = BTreeMap::new();
    let mut unbuilt = 0_usize;
    for lot in &plan.lots {
        let Some(path) = lot.occupant.as_ref() else {
            continue;
        };
        let Some(meta) = tree.file(path) else {
            continue;
        };
        let class = if monument_set.contains(path) {
            FileClass::Monument
        } else {
            meta.class
        };
        let spec = BuildingSpec::new(meta.size_bytes)
            .with_diff_lines(inputs.diff_lines_of(path))
            .with_class(class);
        let area = block_area.get(&lot.block).copied().unwrap_or(f32::MAX);
        match buildings::place_with(lot, path, spec, area) {
            Some(building) => {
                buildings_map.insert(path.clone(), building);
            }
            None => unbuilt += 1,
        }
    }

    // --- Districts, streets, landmarks ------------------------------------
    let districts = build_districts(&block_list, &sites);
    let streets = build_streets(&inputs.streets, &districts, &roads_graph);
    let monuments = monument_marks(tree, &inputs.inbound, &buildings_map);
    let industrial = industrial_masses(tree, &plan.host_of, &districts, &block_list);

    let layout = CityLayout {
        schema: LAYOUT_SCHEMA,
        extent,
        roads: roads_graph,
        blocks: block_list,
        lots: plan.lots,
        buildings: buildings_map,
        districts,
        streets,
    };

    let report = CityReport {
        files: tree.files.len(),
        buildings: layout.buildings.len(),
        blocks: layout.blocks.len(),
        lots: layout.lots.len(),
        empty_lots: layout.lots.iter().filter(|l| l.is_vacant()).count(),
        massed: plan.report.massed,
        overflow: plan.report.overflow,
        unbuilt,
        road_nodes: junctions.nodes,
        road_segments: junctions.segments,
        cycles: junctions.cycles,
        junctions: junctions.junctions,
        complex_junctions: junctions.complex_junctions,
        monuments: monuments.len(),
    };

    City {
        layout,
        terrain: TerrainParams::of(&terrain),
        monuments,
        industrial,
        vacancies: VacancyLedger::new(),
        host_of: plan.host_of,
        overflow: plan.overflow,
        report,
    }
}

/// Promotes segments that leave a district to [`RoadClass::Arterial`].
///
/// PRD §8 makes the district skeleton the wayfinding layer that "must stay
/// readable at all zooms even while the street level tangles", and PRD §12's
/// city zoom draws arterials and drops alleys. A grown network has no such
/// classification of its own, so it is derived here from the one thing that
/// decides placement: which district each end is in (PRD §9).
///
/// A segment whose two ends sit in different districts is an arterial. A
/// segment both of whose ends have no district — a spur into empty ground — is
/// an alley. Everything else is a street.
pub fn classify_roads(graph: &mut RoadGraph, sites: &DistrictSites) {
    if sites.is_empty() {
        return;
    }
    // Resolved per node rather than per segment end, so a node shared by six
    // segments is looked up once and every segment agrees about it.
    let owners: Vec<Option<LogicalPath>> = graph
        .nodes
        .iter()
        .map(|node| sites.district_at(node.position).cloned())
        .collect();
    for segment in &mut graph.segments {
        let from = owners.get(segment.from.0 as usize).and_then(Option::as_ref);
        let to = owners.get(segment.to.0 as usize).and_then(Option::as_ref);
        segment.class = match (from, to) {
            (Some(a), Some(b)) if a != b => RoadClass::Arterial,
            // Both ends outside every district's reach: a spur into empty
            // ground, which PRD §12 drops first when zoomed out.
            (None, None) => RoadClass::Alley,
            _ => RoadClass::Street,
        };
    }
}

/// One [`District`] per district that has ground, in path order.
fn build_districts(block_list: &[Block], sites: &DistrictSites) -> BTreeMap<LogicalPath, District> {
    let grouped = blocks::group_by_district(block_list);
    let by_id: BTreeMap<BlockId, &Block> = block_list.iter().map(|b| (b.id, b)).collect();
    let mut out = BTreeMap::new();
    for (path, ids) in grouped {
        let mut points: Vec<Point> = Vec::new();
        for id in &ids {
            if let Some(block) = by_id.get(id) {
                points.extend(block.boundary.vertices.iter().copied());
            }
        }
        let boundary = convex_hull(&points);
        let centre = sites.centre(&path).unwrap_or_else(|| boundary.centroid());
        out.insert(
            path.clone(),
            District {
                path,
                boundary,
                centre,
                blocks: ids,
            },
        );
    }
    out
}

/// Cross-district import relations, drawn (PRD §9).
///
/// The polyline bends through the road node nearest the midpoint rather than
/// running straight, which is what "following the road network where one
/// exists" buys at this stage: a street that visibly belongs to the city rather
/// than a chord drawn over it. A full shortest path through the graph is a
/// refinement, not a correctness requirement, and it would put a routing
/// algorithm inside the golden file before anyone has looked at one drawn.
fn build_streets(
    relations: &[polis_repo::imports::Street],
    districts: &BTreeMap<LogicalPath, District>,
    graph: &RoadGraph,
) -> Vec<StreetLine> {
    let mut out = Vec::new();
    for relation in relations {
        let (Some(from), Some(to)) = (districts.get(&relation.from), districts.get(&relation.to))
        else {
            continue;
        };
        let midpoint = from.centre.lerp(to.centre, 0.5);
        let mut polyline = vec![from.centre];
        if let Some(via) = nearest_node(graph, midpoint) {
            if via != from.centre && via != to.centre {
                polyline.push(via);
            }
        }
        polyline.push(to.centre);
        out.push(StreetLine {
            from: relation.from.clone(),
            to: relation.to.clone(),
            edge_count: relation.edge_count,
            polyline,
        });
    }
    // `cross_district_edges` is already sorted by `(from, to)`, and the assert
    // is what says so out loud when an upstream change breaks it. But the assert
    // alone is NOT enough: `debug_assert_canonical_order` compiles to nothing
    // when `debug_assertions` is off, so a release build — the one the product
    // ships — would silently serialize `streets` in whatever order the caller
    // supplied, and PRD §15's "byte-identical layout" would fail on a machine
    // that only ever ran `--release`. Verified: reversing `LayoutInputs::streets`
    // used to change the snapshot hash in release and not in debug.
    //
    // So: assert the input order (loudly, in debug, naming the upstream change),
    // then sort unconditionally (cheaply, in every profile, so the output cannot
    // depend on it either way). PRD §7.4 is a property of the artifact, not of
    // the build profile.
    crate::determinism::debug_assert_canonical_order(
        out.iter().map(|s| (s.from.clone(), s.to.clone())),
        "street lines",
    );
    out.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.to.cmp(&b.to)));
    out
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
        // A monument with no building is not a landmark: it is a file in an
        // industrial tree, or one whose parcel was too small for a setback. A
        // label floating over nothing is worse than no label.
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

// ---------------------------------------------------------------------------
// Convex hull
// ---------------------------------------------------------------------------

/// The convex hull of a point set, counter-clockwise (Andrew's monotone chain).
///
/// Used for district and industrial-zone outlines: PRD §8 makes the district
/// skeleton the wayfinding layer, and it must survive being decluttered down to
/// an outline. A hull is the honest shape for that — it is the region the
/// district's ground occupies, it never has holes, and two adjacent districts
/// with interleaved blocks overlap slightly, which is a truthful rendering of
/// interleaved ground rather than a false hard boundary.
///
/// Written out rather than taken from `lyon`: the result is serialized into
/// PRD §16's golden file, and this is a dozen lines of exact `f64` arithmetic
/// with no transcendental function in it ([`crate::determinism`], rules 3 and
/// 4).
#[must_use]
pub fn convex_hull(points: &[Point]) -> Polygon {
    let mut sorted: Vec<Point> = points.iter().copied().filter(|p| p.is_finite()).collect();
    if sorted.len() < 3 {
        return Polygon::new(sorted);
    }
    // A total order over every bit pattern, so the hull does not depend on how
    // the caller happened to collect the points.
    sorted.sort_by(|a, b| a.x.total_cmp(&b.x).then_with(|| a.y.total_cmp(&b.y)));
    // Exact equality is the right test: this removes *duplicate* points, and two
    // points a tolerance apart are two points. An approximate dedup would make
    // the hull depend on which duplicate happened to be first.
    #[allow(clippy::float_cmp)]
    sorted.dedup_by(|a, b| a.x == b.x && a.y == b.y);
    if sorted.len() < 3 {
        return Polygon::new(sorted);
    }

    let mut hull: Vec<Point> = Vec::with_capacity(sorted.len() * 2);
    for pass in 0..2 {
        let start = hull.len();
        let iter: Box<dyn Iterator<Item = &Point>> = if pass == 0 {
            Box::new(sorted.iter())
        } else {
            Box::new(sorted.iter().rev())
        };
        for point in iter {
            while hull.len() >= start + 2 {
                let a = hull[hull.len() - 2];
                let b = hull[hull.len() - 1];
                if turn(a, b, *point) > 0.0 {
                    break;
                }
                hull.pop();
            }
            hull.push(*point);
        }
        // The last point of each pass is the first of the next.
        hull.pop();
    }
    Polygon::new(hull)
}

/// Twice the signed area of the triangle `a, b, c`. Positive for a left turn.
///
/// Written longhand rather than through [`crate::Vec2::cross`], which uses
/// `f32::mul_add` — [`crate::determinism`] rule 5 — and in `f64`, because the
/// sign of this value decides the shape of every district outline.
fn turn(a: Point, b: Point, c: Point) -> f64 {
    let (abx, aby) = (
        f64::from(b.x) - f64::from(a.x),
        f64::from(b.y) - f64::from(a.y),
    );
    let (acx, acy) = (
        f64::from(c.x) - f64::from(a.x),
        f64::from(c.y) - f64::from(a.y),
    );
    abx * acy - aby * acx
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
    /// Files with nowhere to go on the existing lot set, in the order they were
    /// offered. Never dropped: these are what
    /// [`GrowthOutcome::rebuild_required`] is about.
    pub unplaced: Vec<LogicalPath>,
    /// True when the city needs a full [`generate_with`] to house everything.
    ///
    /// The caller should route that regeneration through
    /// [`DeformationLimiter`] rather than applying it immediately (PRD §7.7).
    pub rebuild_required: bool,
}

impl GrowthOutcome {
    /// True when nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.placed == 0 && self.vacated == 0 && self.updated == 0 && self.unplaced.is_empty()
    }
}

/// Applies one growth step for a repository delta.
///
/// Budget: under 50 ms, off-thread (PRD §13.1).
///
/// Removed files leave **vacant lots**, not holes: the lot stays, its occupant
/// becomes `None`, and it goes to seed (PRD §7.5).
///
/// This form drops the vacancy *record* — which lot held what, and since when —
/// because it has no ledger and no time to put in one. Use [`City::grow`] to
/// keep it; that is the form the product uses, and it is the one PRD §7.5's
/// "vacant lots that go to seed over time" needs.
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
/// which is how a rename (a delete plus an add, since `polis-repo` pins
/// `--no-renames`) reuses ground instead of pushing the city outwards.
pub fn step(
    layout: &mut CityLayout,
    ledger: &mut VacancyLedger,
    tree: &RepoTree,
    delta: &RepoDelta,
    inputs: &LayoutInputs,
    when: WallTime,
) -> GrowthOutcome {
    let mut outcome = GrowthOutcome::default();

    // Deletions. Sorted, because `RepoDelta::removed` is a set and a caller's
    // order must not reach the layout (PRD §7.4).
    for path in crate::determinism::canonical_order(delta.removed.iter().cloned()) {
        layout.buildings.remove(&path);
        if ledger.vacate_path(&mut layout.lots, &path, when).is_some() {
            outcome.vacated += 1;
        }
    }

    // Updates: same lot, new footprint and new height.
    let changed = crate::determinism::canonical_order(
        delta.resized.iter().chain(delta.retouched.iter()).cloned(),
    );
    for path in changed {
        if rebuild_building(layout, tree, inputs, &path) {
            outcome.updated += 1;
        }
    }

    // Additions, in the growth order `RepoDelta::added` promises.
    let by_district = lots_by_district(layout);
    for path in &delta.added {
        let candidates = host_candidates(&district_of(path), &by_district);
        let Some(lot_id) = ledger.settle(&mut layout.lots, &candidates, path) else {
            outcome.unplaced.push(path.clone());
            outcome.rebuild_required = true;
            continue;
        };
        if rebuild_building(layout, tree, inputs, path) {
            outcome.placed += 1;
        } else {
            // A parcel too small for a setback: the file has ground but no
            // building, which is the same outcome `generate_with` records as
            // `unbuilt`. Not a rebuild trigger — a regeneration would produce
            // the same sliver.
            let _ = lot_id;
        }
    }

    outcome
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
///
/// The same rule [`crate::lots::plan`] uses — "`src/auth`'s files land in `src`,
/// which is exactly where an operator looks for them" — so an incremental
/// placement and a full re-plan route a file to the same district.
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
    let area = layout
        .block(lot.block)
        .map_or(f32::MAX, |block| block.boundary.area());
    // The landmark class is whatever the tree says. An incremental step does not
    // re-rank monuments: the ranking is a whole-repository property and moving
    // it on one file's arrival would relabel the map under the operator.
    let spec = BuildingSpec::new(meta.size_bytes)
        .with_diff_lines(inputs.diff_lines_of(path))
        .with_class(meta.class);
    if let Some(building) = buildings::place_with(lot, path, spec, area) {
        layout.buildings.insert(path.clone(), building);
        true
    } else {
        // A sliver parcel, or an industrial file: no building, and the stale one
        // (if any) goes rather than lingering at the old size.
        layout.buildings.remove(path);
        false
    }
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

    use std::path::PathBuf;

    use polis_repo::{FileMeta, Language};

    use super::*;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    /// `Duration::from_days` is unstable on the pinned toolchain (1.98).
    fn days(n: u64) -> Duration {
        Duration::from_secs(n * 86_400)
    }

    /// A tree whose growth order, sizes and classes are all written down, so a
    /// change to the city is a change to the algorithm and not to the fixture.
    fn tree_of(files: &[(&str, u64, u32)]) -> RepoTree {
        let mut tree = RepoTree {
            root: PathBuf::from("/fixture"),
            files: BTreeMap::new(),
            worktrees: BTreeMap::new(),
            head: "fixture".to_owned(),
        };
        for (name, size, growth) in files {
            let path = lp(name);
            let mut meta = FileMeta::untracked(path.clone(), *size);
            meta.growth_index = *growth;
            meta.added_at =
                WallTime::from_unix_seconds(1_600_000_000 + i64::from(*growth) * 86_400);
            meta.last_touched = meta.added_at;
            meta.class = polis_repo::tree::classify(&path);
            meta.language = polis_repo::tree::language_for(&path);
            tree.files.insert(path, meta);
        }
        tree
    }

    /// A repository big enough to close loops in the road graph.
    fn town() -> RepoTree {
        let mut files: Vec<(String, u64, u32)> = Vec::new();
        let dirs = [
            "src", "src/auth", "src/net", "src/ui", "tests", "docs", "examples",
        ];
        let mut growth = 0_u32;
        for (d, dir) in dirs.iter().enumerate() {
            let d = d as u64;
            for i in 0_u64..14 {
                files.push((
                    format!("{dir}/file{i:02}.rs"),
                    600 + (d * 97 + i * 311) % 40_000,
                    growth,
                ));
                growth += 1;
            }
        }
        files.push(("README.md".to_owned(), 4_000, growth));
        files.push(("src/main.rs".to_owned(), 3_000, growth + 1));
        files.push(("src/lib.rs".to_owned(), 9_000, growth + 2));
        files.push(("node_modules/left-pad/index.js".to_owned(), 900, growth + 3));
        files.push(("node_modules/left-pad/pkg.js".to_owned(), 700, growth + 4));
        let borrowed: Vec<(&str, u64, u32)> =
            files.iter().map(|(n, s, g)| (n.as_str(), *s, *g)).collect();
        tree_of(&borrowed)
    }

    // -----------------------------------------------------------------------
    // The pipeline (PRD §7.2)
    // -----------------------------------------------------------------------

    #[test]
    fn an_empty_repository_produces_an_empty_city_rather_than_a_panic() {
        let city = generate_city(&RepoTree::default());
        assert_eq!(city.report.files, 0);
        assert_eq!(city.report.buildings, 0);
        assert!(city.layout.buildings.is_empty());
        assert!(city.snapshot().is_ok());
        assert_eq!(city.layout.schema, LAYOUT_SCHEMA);
    }

    #[test]
    fn a_one_file_repository_still_serializes() {
        let tree = tree_of(&[("README.md", 400, 0)]);
        let city = generate_city(&tree);
        assert_eq!(city.report.files, 1);
        let json = city.snapshot().expect("serializes");
        assert!(json.contains("\"format\": 1"));
    }

    /// PRD §7.2's organic signature, measured at the top of the pipeline. A tree
    /// compiles, runs, renders — and looks wrong.
    #[test]
    fn the_generated_city_is_not_a_tree() {
        let city = generate_city(&town());
        assert!(
            city.report.cycles > 0,
            "the road graph is a tree: {} nodes, {} segments, 0 cycles. \
             PRD §7.2 — without snapping you get a tree, and trees read as \
             artificial.",
            city.report.road_nodes,
            city.report.road_segments
        );
        assert!(city.report.blocks > 0, "no closed loop became a block");
        assert!(
            city.report.complex_junctions > 0,
            "no four-way junction anywhere: {:?}",
            city.report
        );
    }

    #[test]
    fn every_file_gets_a_building_a_mass_or_a_reason() {
        let tree = town();
        let city = generate_city(&tree);
        let accounted =
            city.report.buildings + city.report.massed + city.report.overflow + city.report.unbuilt;
        assert!(
            accounted >= city.report.files.saturating_sub(1),
            "{accounted} of {} files accounted for: {:?}",
            city.report.files,
            city.report
        );
        // PRD §8: node_modules is one dull mass, not two buildings.
        assert_eq!(city.report.massed, 2);
        assert!(!city.industrial.is_empty());
        assert!(city
            .layout
            .buildings
            .keys()
            .all(|p| !p.as_str().starts_with("node_modules/")));
    }

    #[test]
    fn buildings_stand_on_their_own_lots_and_inside_their_blocks() {
        let city = generate_city(&town());
        for building in city.layout.buildings.values() {
            let lot = city
                .layout
                .lot(building.lot)
                .expect("a building's lot exists");
            assert_eq!(lot.occupant.as_ref(), Some(&building.path));
            assert!(building.footprint.is_valid());
            assert!(building.footprint.area() > 0.0);
            assert!(
                building.footprint.area() <= lot.boundary.area() + 1e-3,
                "{} covers more than its lot",
                building.path.as_str()
            );
            assert!(building.height >= buildings::BASE_HEIGHT);
            assert!(building.rotation.abs() <= MAX_ROTATION_RADIANS);
        }
    }

    const MAX_ROTATION_RADIANS: f32 = 0.070;

    #[test]
    fn districts_carry_their_blocks_and_a_finite_centre() {
        let city = generate_city(&town());
        assert!(city.layout.districts.len() > 3, "a town has districts");
        for (path, district) in &city.layout.districts {
            assert_eq!(&district.path, path);
            assert!(district.centre.is_finite());
            assert!(!district.blocks.is_empty());
            assert!(district.boundary.is_valid());
        }
    }

    #[test]
    fn road_classes_separate_the_wayfinding_layer_from_the_street_level() {
        let city = generate_city(&town());
        let arterials = city
            .layout
            .roads
            .segments
            .iter()
            .filter(|s| s.class == RoadClass::Arterial)
            .count();
        let streets = city
            .layout
            .roads
            .segments
            .iter()
            .filter(|s| s.class == RoadClass::Street)
            .count();
        assert!(arterials > 0, "no road leaves a district");
        assert!(streets > 0, "no road stays inside one");
        assert!(
            arterials < streets,
            "arterials must be the skeleton, not the network: {arterials} vs {streets}"
        );
    }

    // -----------------------------------------------------------------------
    // Landmarks (PRD §8)
    // -----------------------------------------------------------------------

    #[test]
    fn monuments_are_ranked_capped_and_actually_placed() {
        let tree = town();
        let inbound = vec![(lp("src/lib.rs"), 40), (lp("src/auth/file00.rs"), 12)];
        let city = generate_with(
            &tree,
            &LayoutInputs {
                inbound,
                ..LayoutInputs::default()
            },
        );
        assert!(!city.monuments.is_empty());
        assert!(city.monuments.len() <= MAX_MONUMENTS);
        for (i, m) in city.monuments.iter().enumerate() {
            assert_eq!(m.rank as usize, i, "ranks are dense and ordered");
            assert!(
                city.layout.buildings.contains_key(&m.path),
                "a label floating over nothing: {}",
                m.path.as_str()
            );
        }
        // A monument is tall, whatever its diff says.
        for m in &city.monuments {
            let b = &city.layout.buildings[&m.path];
            assert!(b.height >= buildings::MONUMENT_HEIGHT);
            assert_eq!(b.roof, RoofForm::Stepped);
        }
        assert!(city.is_monument(&lp("src/main.rs")));
    }

    #[test]
    fn an_industrial_tree_is_one_mass_with_a_host_and_a_shape() {
        let city = generate_city(&town());
        let mass = city
            .industrial
            .iter()
            .find(|m| m.district.as_str() == "node_modules/left-pad")
            .expect("node_modules is industrial");
        assert_eq!(mass.files, 2);
        assert_eq!(mass.bytes, 1_600);
        assert!(
            city.layout.districts.contains_key(&mass.host),
            "the mass is drawn over a district that has ground"
        );
        assert!(mass.boundary.is_valid());
    }

    // -----------------------------------------------------------------------
    // Determinism (PRD §7.4, §15, §16)
    // -----------------------------------------------------------------------

    #[test]
    fn two_generations_of_the_same_tree_are_byte_identical() {
        let tree = town();
        let first = generate_city(&tree).snapshot().expect("serializes");
        for _ in 0..4 {
            let again = generate_city(&tree).snapshot().expect("serializes");
            assert_eq!(first.len(), again.len());
            assert!(first == again, "the city moved between two runs");
        }
    }

    /// The failure PRD §7.4 names first: iteration order reaching the layout.
    /// A `HashMap`'s order is randomised per process by `RandomState`, so a
    /// pipeline that depended on the order its input was *handed to it* would
    /// produce a different city per launch. Feeding the same files through a
    /// `HashMap` is the cheapest way to prove it does not.
    #[test]
    fn input_order_cannot_move_the_city() {
        let ordered = town();
        let mut shuffled = RepoTree {
            root: ordered.root.clone(),
            files: BTreeMap::new(),
            worktrees: BTreeMap::new(),
            head: ordered.head.clone(),
        };
        // Round-tripping through a `HashMap` reorders the insertions by
        // `RandomState`, which differs on every process launch.
        let scrambled: std::collections::HashMap<LogicalPath, FileMeta> = ordered
            .files
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (path, meta) in scrambled {
            shuffled.files.insert(path, meta);
        }
        assert_eq!(
            generate_city(&ordered).snapshot().expect("serializes"),
            generate_city(&shuffled).snapshot().expect("serializes"),
        );
    }

    #[test]
    fn the_snapshot_prints_every_float_at_three_decimals() {
        let city = generate_city(&town());
        let json = city.snapshot().expect("serializes");
        let mut checked = 0_usize;
        for token in json.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-')) {
            let Some((_, frac)) = token.split_once('.') else {
                continue;
            };
            if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            assert!(
                frac.len() <= SNAPSHOT_DECIMALS as usize,
                "{token} has more than {SNAPSHOT_DECIMALS} decimals; a last-ulp \
                 difference between two machines would break PRD §16's byte \
                 comparison"
            );
            checked += 1;
        }
        assert!(checked > 100, "only {checked} floats found in the snapshot");
        assert!(!json.contains("NaN") && !json.contains("null,\n    \"x\""));
    }

    /// The trap this constant exists to avoid: `1 / QUANTUM` is not 1000.
    #[test]
    fn snapshot_scale_is_exact() {
        assert_eq!(PRINT_SCALE, 1_000.0);
        assert_ne!(1.0 / f64::from(QUANTUM), PRINT_SCALE);
        assert_eq!(
            serde_json::to_string(&Q(QUANTUM)).expect("serializes"),
            "0.001"
        );
        assert_eq!(serde_json::to_string(&Q(-0.0)).expect("serializes"), "0.0");
        assert_eq!(
            serde_json::to_string(&Q(f32::NAN)).expect("serializes"),
            "0.0",
            "a non-finite coordinate is a bug upstream; it must not become an \
             unreadable golden file"
        );
        assert_eq!(
            serde_json::to_string(&Q(1.234_567)).expect("serializes"),
            "1.235"
        );
        assert_eq!(
            serde_json::to_string(&P::of(Point::new(-12.3456, 7.0))).expect("serializes"),
            "[-12.346,7.0]"
        );
    }

    #[test]
    fn quantized_is_idempotent_and_leaves_no_negative_zero() {
        let city = generate_city(&town());
        let once = quantized(&city.layout);
        let twice = quantized(&once);
        assert_eq!(
            snapshot(&once).expect("serializes"),
            snapshot(&twice).expect("serializes")
        );
        for node in &once.roads.nodes {
            assert!(node.position.x.is_finite() && node.position.y.is_finite());
            assert!(!node.position.x.is_sign_negative() || node.position.x != 0.0);
        }
    }

    #[test]
    fn the_terrain_digest_is_in_the_snapshot_and_moves_with_the_seed() {
        let tree = town();
        let a = generate_with_seed(&tree, &LayoutInputs::default(), TERRAIN_SEED);
        let b = generate_with_seed(&tree, &LayoutInputs::default(), TERRAIN_SEED ^ 1);
        assert_ne!(a.terrain.digest, b.terrain.digest);
        assert_ne!(
            a.snapshot().expect("serializes"),
            b.snapshot().expect("serializes"),
            "a different landscape must be a different city"
        );
        assert!(a
            .snapshot()
            .expect("serializes")
            .contains(&hex64(a.terrain.digest)));
    }

    #[test]
    fn the_digest_matches_the_snapshot_it_claims_to_summarise() {
        let city = generate_city(&town());
        let json = city.snapshot().expect("serializes");
        assert_eq!(city.digest(), fnv1a64(json.as_bytes()));
        assert_ne!(
            city.digest(),
            generate_city(&tree_of(&[("a.rs", 1, 0)])).digest()
        );
    }

    // -----------------------------------------------------------------------
    // Incremental growth (PRD §7.4)
    // -----------------------------------------------------------------------

    #[test]
    fn a_deleted_file_leaves_a_vacant_lot_not_a_hole() {
        let tree = town();
        let mut city = generate_city(&tree);
        let victim = city
            .layout
            .buildings
            .keys()
            .next()
            .cloned()
            .expect("a building");
        let lots_before = city.layout.lots.len();

        let delta = RepoDelta {
            removed: vec![victim.clone()],
            ..RepoDelta::default()
        };
        let when = WallTime::from_unix_seconds(1_700_000_000);
        let outcome = city.grow(&tree, &delta, &LayoutInputs::default(), when);

        assert_eq!(outcome.vacated, 1);
        assert_eq!(city.layout.lots.len(), lots_before, "the lot stays");
        assert!(city.layout.building(&victim).is_none());
        assert_eq!(city.vacancies.len(), 1);
        let vacancy = city.vacancies.iter().next().expect("a record");
        assert_eq!(vacancy.former, victim);
        assert_eq!(vacancy.since, when);
        // PRD §7.5: it goes to seed over time, and the curve is a render input.
        assert_eq!(vacancy.seed_progress(when), 0.0);
        assert!(vacancy.seed_progress(when.saturating_add(days(200))) > 0.9);
    }

    /// The property that makes "growth is genuinely incremental" true rather
    /// than aspirational: over one lot set, settling a file where a full re-plan
    /// would have put it.
    #[test]
    fn a_re_added_file_returns_to_the_lot_it_left() {
        let tree = town();
        let mut city = generate_city(&tree);
        let path = city
            .layout
            .buildings
            .keys()
            .nth(3)
            .cloned()
            .expect("a building");
        let before = city.layout.building(&path).cloned().expect("a building");

        let when = WallTime::from_unix_seconds(1_700_000_000);
        city.grow(
            &tree,
            &RepoDelta {
                removed: vec![path.clone()],
                ..RepoDelta::default()
            },
            &LayoutInputs::default(),
            when,
        );
        let outcome = city.grow(
            &tree,
            &RepoDelta {
                added: vec![path.clone()],
                ..RepoDelta::default()
            },
            &LayoutInputs::default(),
            when,
        );

        assert_eq!(outcome.placed, 1);
        let after = city.layout.building(&path).expect("rebuilt");
        assert_eq!(&before, after, "the file came back to a different lot");
        assert!(city.vacancies.is_empty(), "the vacancy record is cleared");
    }

    #[test]
    fn a_resize_changes_the_footprint_and_nothing_else_moves() {
        let mut tree = town();
        let mut city = generate_city(&tree);
        // The building with the most room, so the footprint is genuinely
        // size-limited rather than sitting on `lots::MIN_LOT_AREA`.
        let path = city
            .layout
            .buildings
            .values()
            .max_by(|a, b| a.footprint.area().total_cmp(&b.footprint.area()))
            .expect("a building")
            .path
            .clone();
        let before = city.layout.building(&path).cloned().expect("a building");
        let others_before: Vec<_> = city
            .layout
            .buildings
            .iter()
            .filter(|(p, _)| **p != path)
            .map(|(p, b)| (p.clone(), b.clone()))
            .collect();

        // Shrunk, not grown: at `buildings::FOOTPRINT_SCALE` an ordinary source
        // file already fills its lot, and a *bigger* file would correctly change
        // nothing — the lot is the constraint. Shrinking exercises the same path
        // and cannot be defeated by the clamp.
        tree.files.get_mut(&path).expect("in the tree").size_bytes = 120;
        city.grow(
            &tree,
            &RepoDelta {
                resized: vec![path.clone()],
                ..RepoDelta::default()
            },
            &LayoutInputs::default(),
            WallTime::UNIX_EPOCH,
        );

        let after = city.layout.building(&path).expect("still there");
        assert!(
            after.footprint.area() < before.footprint.area(),
            "{} -> {}",
            before.footprint.area(),
            after.footprint.area()
        );
        assert_eq!(after.lot, before.lot, "a resize never moves a building");
        for (p, b) in others_before {
            assert_eq!(&b, city.layout.building(&p).expect("untouched"));
        }
    }

    #[test]
    fn a_brand_new_file_settles_without_regenerating_the_world() {
        let mut tree = town();
        let path = lp("src/net/added.rs");
        let mut meta = FileMeta::untracked(path.clone(), 5_000);
        meta.growth_index = 9_000;
        meta.language = Some(Language::Rust);
        tree.files.insert(path.clone(), meta);

        let mut city = generate_city(&town());
        let roads_before = city.layout.roads.segments.len();
        let outcome = city.grow(
            &tree,
            &RepoDelta {
                added: vec![path.clone()],
                ..RepoDelta::default()
            },
            &LayoutInputs::default(),
            WallTime::UNIX_EPOCH,
        );

        assert_eq!(outcome.placed, 1);
        assert!(!outcome.rebuild_required);
        assert_eq!(city.layout.roads.segments.len(), roads_before);
        let building = city.layout.building(&path).expect("built");
        let lot = city.layout.lot(building.lot).expect("its lot");
        let block = city.layout.block(lot.block).expect("its block");
        assert!(
            block.district.as_str().starts_with("src"),
            "a new file must land in its own district, not across town: {}",
            block.district.as_str()
        );
    }

    #[test]
    fn a_file_with_nowhere_to_go_asks_for_a_rebuild_rather_than_vanishing() {
        let tree = tree_of(&[("a.rs", 100, 0)]);
        let mut layout = CityLayout::default();
        let delta = RepoDelta {
            added: vec![lp("a.rs")],
            ..RepoDelta::default()
        };
        let mut ledger = VacancyLedger::new();
        let outcome = step(
            &mut layout,
            &mut ledger,
            &tree,
            &delta,
            &LayoutInputs::default(),
            WallTime::UNIX_EPOCH,
        );
        assert!(outcome.rebuild_required);
        assert_eq!(outcome.unplaced, vec![lp("a.rs")]);
        assert!(!outcome.is_empty());
    }

    #[test]
    fn the_bare_grow_form_still_empties_a_lot() {
        let tree = town();
        let mut layout = generate(&tree);
        let victim = layout.buildings.keys().next().cloned().expect("a building");
        grow(
            &mut layout,
            &tree,
            &RepoDelta {
                removed: vec![victim.clone()],
                ..RepoDelta::default()
            },
        );
        assert!(layout.building(&victim).is_none());
        assert!(layout
            .lots
            .iter()
            .all(|l| l.occupant.as_ref() != Some(&victim)));
    }

    // -----------------------------------------------------------------------
    // Deformation rate limiting (PRD §7.7)
    // -----------------------------------------------------------------------

    fn added(paths: &[&str]) -> RepoDelta {
        RepoDelta {
            added: paths.iter().map(|p| lp(p)).collect(),
            ..RepoDelta::default()
        }
    }

    #[test]
    fn a_still_camera_applies_and_a_moving_one_defers() {
        let mut limiter = DeformationLimiter::new();
        assert!(limiter.is_empty());
        assert!(!limiter.submit(added(&["a.rs"]), Duration::ZERO));
        assert!(!limiter.submit(added(&["b.rs"]), Duration::from_millis(100)));
        assert_eq!(limiter.pending().added.len(), 2);
        assert!(limiter.submit(added(&["c.rs"]), Duration::from_millis(CAMERA_STILL_MS)));
        let batch = limiter.take_pending();
        assert_eq!(batch.added.len(), 3, "a burst becomes one change");
        assert!(limiter.is_empty());
        assert_eq!(limiter.deferrals(), 0);
    }

    #[test]
    fn an_operator_who_never_stops_panning_still_gets_their_map() {
        let mut limiter = DeformationLimiter::new();
        let mut applied = false;
        for i in 0..(MAX_DEFERRALS + 2) {
            if limiter.submit(added(&[&format!("f{i}.rs")]), Duration::ZERO) {
                applied = true;
                break;
            }
        }
        assert!(applied, "the batch starved");
        assert!(limiter.deferrals() >= MAX_DEFERRALS);
    }

    #[test]
    fn a_big_enough_batch_applies_whatever_the_camera_is_doing() {
        let mut limiter = DeformationLimiter::new();
        let paths: Vec<String> = (0..MAX_DEFERRED_CHANGES)
            .map(|i| format!("f{i}.rs"))
            .collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        assert!(limiter.submit(added(&refs), Duration::ZERO));
    }

    #[test]
    fn the_tween_is_never_shorter_than_the_prd_floor() {
        let mut limiter = DeformationLimiter::new();
        limiter.submit(added(&["a.rs"]), Duration::ZERO);
        assert!(limiter.tween() >= Duration::from_millis(MIN_TWEEN_MS));
        let many: Vec<String> = (0..500).map(|i| format!("f{i}.rs")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        limiter.submit(added(&refs), Duration::ZERO);
        assert!(limiter.tween() > Duration::from_millis(MIN_TWEEN_MS));
        assert!(limiter.tween() <= Duration::from_millis(MAX_TWEEN_MS));
    }

    #[test]
    fn a_file_created_and_deleted_inside_one_batch_never_reaches_the_map() {
        let mut limiter = DeformationLimiter::new();
        limiter.submit(added(&["scratch.tmp", "real.rs"]), Duration::ZERO);
        limiter.submit(
            RepoDelta {
                removed: vec![lp("scratch.tmp")],
                ..RepoDelta::default()
            },
            Duration::ZERO,
        );
        let batch = limiter.take_pending();
        assert_eq!(batch.added, vec![lp("real.rs")]);
        assert!(batch.removed.is_empty());
    }

    #[test]
    fn a_merged_batch_does_not_depend_on_submission_order() {
        let one = {
            let mut l = DeformationLimiter::new();
            l.submit(added(&["a.rs", "b.rs"]), Duration::ZERO);
            l.submit(
                RepoDelta {
                    resized: vec![lp("z.rs"), lp("y.rs")],
                    retouched: vec![lp("x.rs")],
                    ..RepoDelta::default()
                },
                Duration::ZERO,
            );
            l.take_pending()
        };
        let two = {
            let mut l = DeformationLimiter::new();
            l.submit(
                RepoDelta {
                    resized: vec![lp("y.rs"), lp("z.rs")],
                    retouched: vec![lp("x.rs")],
                    ..RepoDelta::default()
                },
                Duration::ZERO,
            );
            l.submit(added(&["a.rs", "b.rs"]), Duration::ZERO);
            l.take_pending()
        };
        assert_eq!(one.resized, two.resized);
        assert_eq!(one.retouched, two.retouched);
        assert_eq!(one.added, two.added);
    }

    #[test]
    fn a_removed_then_re_added_file_is_a_change_not_a_new_building() {
        let mut limiter = DeformationLimiter::new();
        limiter.submit(
            RepoDelta {
                removed: vec![lp("src/a.rs")],
                ..RepoDelta::default()
            },
            Duration::ZERO,
        );
        limiter.submit(added(&["src/a.rs"]), Duration::ZERO);
        let batch = limiter.take_pending();
        assert!(batch.removed.is_empty());
        assert!(batch.added.is_empty());
        assert_eq!(batch.resized, vec![lp("src/a.rs")]);
    }

    // -----------------------------------------------------------------------
    // Convex hull
    // -----------------------------------------------------------------------

    #[test]
    fn the_hull_of_a_square_with_an_interior_point_is_the_square() {
        let hull = convex_hull(&[
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(10.0, 10.0),
            Point::new(0.0, 10.0),
            Point::new(5.0, 5.0),
            Point::new(2.0, 3.0),
        ]);
        assert_eq!(hull.len(), 4);
        assert!((hull.area() - 100.0).abs() < 1e-3);
        assert!(hull.is_ccw());
    }

    #[test]
    fn the_hull_is_independent_of_input_order() {
        let base: Vec<Point> = (0..40_u16)
            .map(|i| {
                let t = f32::from(i) * 0.37;
                Point::new(t.sin() * 10.0, t.cos() * 7.0)
            })
            .collect();
        let mut reversed = base.clone();
        reversed.reverse();
        assert_eq!(convex_hull(&base), convex_hull(&reversed));
    }

    #[test]
    fn degenerate_hulls_do_not_panic() {
        assert!(convex_hull(&[]).is_empty());
        assert_eq!(convex_hull(&[Point::ORIGIN]).len(), 1);
        assert_eq!(convex_hull(&[Point::ORIGIN, Point::new(1.0, 1.0)]).len(), 2);
        // Collinear points enclose no area and must not loop forever.
        let line: Vec<Point> = (0..10_u16).map(|i| Point::new(f32::from(i), 0.0)).collect();
        let hull = convex_hull(&line);
        assert!(hull.area() < 1e-6);
        // A non-finite point is dropped rather than poisoning the hull.
        let poisoned = convex_hull(&[
            Point::new(f32::NAN, 0.0),
            Point::ORIGIN,
            Point::new(4.0, 0.0),
            Point::new(0.0, 4.0),
        ]);
        assert!(poisoned.vertices.iter().all(|p| p.is_finite()));
    }

    // -----------------------------------------------------------------------
    // Streets (PRD §9)
    // -----------------------------------------------------------------------

    #[test]
    fn streets_join_districts_that_exist_and_skip_ones_that_do_not() {
        let tree = town();
        let inputs = LayoutInputs {
            streets: vec![
                polis_repo::imports::Street {
                    from: lp("src/auth"),
                    to: lp("src/net"),
                    edge_count: 5,
                },
                polis_repo::imports::Street {
                    from: lp("src/auth"),
                    to: lp("nowhere"),
                    edge_count: 2,
                },
            ],
            ..LayoutInputs::default()
        };
        let city = generate_with(&tree, &inputs);
        assert_eq!(
            city.layout.streets.len(),
            1,
            "a street to nowhere is not drawn"
        );
        let street = &city.layout.streets[0];
        assert_eq!(street.edge_count, 5);
        assert!(street.polyline.len() >= 2);
        assert!(street.polyline.iter().all(|p| p.is_finite()));
    }
}
