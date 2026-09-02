//! Step 1 — the terrain field (PRD §7.2).
//!
//! > Low-frequency simplex noise, biased by directory depth (deeper = "higher
//! > ground"). **Never rendered directly.** Its only job is to give roads
//! > contours to follow, so curvature looks justified rather than randomly
//! > wiggled. Cheapest source of organic-ness available.
//!
//! Layer 1 of PRD §10.3 is "terrain / vacant lots — barely visible"; that is the
//! vacant lots, not this field. The field itself is an input to [`crate::roads`]
//! and nothing else, and it is deliberately absent from
//! [`crate::CityLayout`] — it is reproducible from its seed, so serializing it
//! would only make the golden files larger and the diffs less readable.
//!
//! # The depth bias is what makes it mean something
//!
//! Plain noise gives roads *a* contour to follow. Biasing height by directory
//! depth makes the contour carry information: deep directories sit on high
//! ground, the roads that reach them wind, and the age structure PRD §7.1
//! establishes gets a second, independent visual channel. The bias is a
//! **function of the district**, so it is applied per district centre and
//! interpolated, not sampled per point — sampling per point would need a path
//! lookup inside the road-growth inner loop.
//!
//! # Units, so that a slope threshold means something
//!
//! Height is in **city-space units**, the same units as [`crate::Point`]: the
//! base relief is [`RELIEF_FRACTION`] of the city's extent. That makes
//! [`TerrainField::slope`] a plain dimensionless rise-over-run, so
//! A slope threshold expressed as a fraction of the relief is a number a human can
//! reason about (`0.3` is a noticeable hill) and does not need retuning when the
//! city's extent changes.
//!
//! # Determinism
//!
//! The field is a pure function of `(seed, extent, the set of biased
//! districts)`. In particular it is **independent of the order**
//! [`TerrainField::bias_district`] is called in: the biases are kept sorted by
//! logical path and summed in that canonical order, so a caller that iterates a
//! `HashMap` of districts still gets the same terrain — and therefore the same
//! roads, blocks, lots and buildings — on every run and every machine
//! (PRD §7.4, rule 2 in [`crate::determinism`]). Biasing the same district twice
//! replaces the first bias rather than adding to it, so an incremental growth
//! step cannot drift away from a full regeneration.
//!
//! All arithmetic is `f64` and every function contains only `+ - * /`,
//! comparison, `floor` and `sqrt`. There is no transcendental function anywhere
//! in the field, which is what makes it bit-identical across platforms without
//! relying on quantisation.

use polis_events::LogicalPath;

use crate::determinism::{
    debug_assert_canonical_order, fbm2_with_gradient, narrow, TERRAIN_OCTAVES,
};
use crate::{Point, Vec2};

/// Base relief as a fraction of the city's extent.
///
/// The noise alone moves the ground by `±RELIEF_FRACTION * extent`. Small on
/// purpose: PRD §7.2 wants contours for roads to follow, not mountains.
pub const RELIEF_FRACTION: f64 = 0.15;

/// How many features of the lowest octave span the city.
///
/// "Low-frequency" made concrete: about three broad rises across the whole map.
/// Raising this is the fastest way to turn PRD §7's grown city into the "noise
/// sprinkled on a grid" it explicitly is not.
pub const FEATURES_ACROSS_CITY: f64 = 3.0;

/// A district's bias radius, as a fraction of the city's extent.
pub const DISTRICT_RADIUS_FRACTION: f64 = 0.35;

/// The tallest a district's depth bias can push the ground, as a fraction of the
/// city's extent.
///
/// Comparable to [`RELIEF_FRACTION`], so a deep district reads as high ground
/// without erasing the noise contours underneath it.
pub const DEPTH_RELIEF_FRACTION: f64 = 0.12;

/// Depth at which the depth bias reaches half of [`DEPTH_RELIEF_FRACTION`].
///
/// The bias saturates rather than growing without bound: `node_modules`
/// vendoring at depth 14 must not become a mountain that dwarfs the repository
/// it lives in.
const DEPTH_HALF_SATURATION: f64 = 3.0;

/// Extent substituted when a caller passes a degenerate one.
///
/// A zero or non-finite extent would put a division by zero in the frequency,
/// and a `NaN` terrain silently deletes every road. Degrading to a unit city is
/// wrong but visible; a `NaN` is wrong and invisible.
const FALLBACK_EXTENT: f64 = 1.0;

/// The depth bias as a fraction of the city extent — `0` at the repository root,
/// saturating towards [`DEPTH_RELIEF_FRACTION`].
///
/// `d / (d + k)`: a rational function, so it is exact IEEE arithmetic rather
/// than a `powf` or a `ln` whose last bit is a platform's choice
/// ([`crate::determinism`], rule 4).
#[must_use]
pub fn depth_bias_fraction(depth: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)] // a path depth is far below 2^53
    let depth = depth as f64;
    DEPTH_RELIEF_FRACTION * depth / (depth + DEPTH_HALF_SATURATION)
}

/// One district's contribution to the height field.
#[derive(Debug, Clone, PartialEq)]
struct DistrictBias {
    /// The district. Also the sort key that makes the accumulation order
    /// canonical.
    path: LogicalPath,
    centre_x: f64,
    centre_y: f64,
    /// Peak height added at the centre, in city-space units.
    height: f64,
    /// Squared influence radius, so the falloff needs no `sqrt`.
    radius_sq: f64,
}

impl DistrictBias {
    /// The falloff weight and its two partial derivatives at a point.
    ///
    /// `w = q³` where `q = 1 - dist² / r²`, clipped to zero outside the radius.
    /// Chosen over anything involving a distance because it needs no `sqrt`, and
    /// over a linear falloff because `q³` has zero derivative at both the centre
    /// and the rim: a district boundary must not put a crease in the ground that
    /// roads then follow, which would draw the district outline in streets.
    ///
    /// ```text
    /// ∂q/∂x = -2·(x - cx) / r²      ∂w/∂x = 3·q²·∂q/∂x
    /// ```
    #[inline]
    fn weight_and_gradient(&self, x: f64, y: f64) -> (f64, f64, f64) {
        let dx = x - self.centre_x;
        let dy = y - self.centre_y;
        let q = 1.0 - (dx * dx + dy * dy) / self.radius_sq;
        if q <= 0.0 {
            return (0.0, 0.0, 0.0);
        }
        let common = -6.0 * q * q / self.radius_sq;
        (q * q * q, common * dx, common * dy)
    }
}

/// A scalar height field over city space.
///
/// Not serialized: it is reproducible from [`TerrainField::seed`] and the
/// district bias, and PRD §16's golden files are more useful without it.
///
/// [`TerrainField::default`] is a **flat** field — height zero and gradient zero
/// everywhere — so a module that forgets to run step 1 gets straight roads and
/// an obvious visual bug, rather than a plausible-looking city grown from an
/// accidental seed.
#[derive(Debug, Clone, Default)]
pub struct TerrainField {
    seed: u64,
    extent: f64,
    /// Multiplies a city-space coordinate on the way into the noise.
    frequency: f64,
    /// The noise's peak height, in city-space units. Zero for a flat field.
    relief: f64,
    octaves: u32,
    /// Sorted by [`DistrictBias::path`]. The sort is what makes the field
    /// independent of the order districts were biased in.
    biases: Vec<DistrictBias>,
}

impl TerrainField {
    /// Builds the field for a repository.
    ///
    /// Seeded from the repository root path, never from the clock (PRD §7.4).
    /// `extent` is [`crate::CityLayout::extent`] — the half-width of the square
    /// the city occupies.
    ///
    /// A zero or non-finite `extent` degrades to a unit city rather than
    /// producing a `NaN` field.
    #[must_use]
    pub fn generate(seed: u64, extent: f32) -> Self {
        let extent = f64::from(extent).abs();
        let extent = if extent > 0.0 && extent.is_finite() {
            extent
        } else {
            FALLBACK_EXTENT
        };
        Self {
            seed,
            extent,
            // The city spans `2 * extent`, and one noise unit is one feature.
            frequency: FEATURES_ACROSS_CITY / (2.0 * extent),
            relief: RELIEF_FRACTION * extent,
            octaves: TERRAIN_OCTAVES,
            biases: Vec::new(),
        }
    }

    /// Adds a district's depth bias at a position.
    ///
    /// Called once per district after [`TerrainField::generate`], from
    /// [`crate::city`]. Deeper directories are higher ground.
    ///
    /// Idempotent per district: biasing the same district again **replaces** the
    /// previous bias, so an incremental step that re-biases a district whose
    /// centre moved cannot leave a ghost hill behind. Call order does not affect
    /// the resulting field.
    ///
    /// A district at the repository root (depth 0) and a non-finite centre are
    /// both no-ops: the first adds nothing by construction, the second would
    /// poison the whole field.
    pub fn bias_district(&mut self, district: &LogicalPath, centre: Point) {
        if !centre.is_finite() {
            return;
        }
        let height = depth_bias_fraction(district.depth()) * self.extent;
        let radius = DISTRICT_RADIUS_FRACTION * self.extent;
        let bias = DistrictBias {
            path: district.clone(),
            centre_x: f64::from(centre.x),
            centre_y: f64::from(centre.y),
            height,
            radius_sq: radius * radius,
        };
        match self
            .biases
            .binary_search_by(|existing| existing.path.cmp(&bias.path))
        {
            Ok(index) => self.biases[index] = bias,
            Err(index) => self.biases.insert(index, bias),
        }
        // The insert above maintains the invariant; this is the guard that says
        // so out loud, and the one that would fire if a later edit replaced the
        // binary search with a push.
        debug_assert_canonical_order(
            self.biases.iter().map(|bias| &bias.path),
            "terrain district biases",
        );
    }

    /// The seed the field was generated from.
    #[must_use]
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// The half-width of the square the city occupies, as
    /// [`TerrainField::generate`] resolved it.
    #[must_use]
    pub fn extent(&self) -> f32 {
        narrow(self.extent)
    }

    /// Octaves of noise in the field — [`TERRAIN_OCTAVES`], or `0` for a flat
    /// [`TerrainField::default`].
    #[must_use]
    pub fn octaves(&self) -> u32 {
        self.octaves
    }

    /// Peak height of the base noise, in city-space units.
    #[must_use]
    pub fn relief(&self) -> f32 {
        narrow(self.relief)
    }

    /// How many districts have been biased.
    #[must_use]
    pub fn district_count(&self) -> usize {
        self.biases.len()
    }

    /// True for a field that is exactly flat everywhere — the default field, or
    /// one generated with no relief.
    #[must_use]
    pub fn is_flat(&self) -> bool {
        (self.relief == 0.0 || self.octaves == 0) && self.biases.iter().all(|b| b.height == 0.0)
    }

    /// Height at a point.
    ///
    /// In city-space units: the same units as [`Point`], so a height and a
    /// distance are comparable and [`TerrainField::slope`] is dimensionless.
    #[must_use]
    pub fn height(&self, at: Point) -> f32 {
        narrow(self.height_f64(f64::from(at.x), f64::from(at.y)))
    }

    /// Downhill gradient at a point.
    ///
    /// Road segments follow this where the slope exceeds a threshold, which is
    /// what makes the curvature read as justified rather than decorative. On the
    /// hot path of road growth: called once per candidate segment per step.
    ///
    /// **Points downhill**, i.e. it is `-∇height`, the direction water runs.
    /// [`crate::determinism::fbm2_gradient`] returns the mathematical (uphill)
    /// gradient; this negates it, because a road that follows a contour follows
    /// the descent. Its length is [`TerrainField::slope`].
    #[must_use]
    pub fn gradient(&self, at: Point) -> Vec2 {
        let (dx, dy) = self.gradient_f64(f64::from(at.x), f64::from(at.y));
        Vec2::new(narrow(dx), narrow(dy))
    }

    /// Slope magnitude at a point — `gradient(at).length()`, but without
    /// building the vector when only the comparison against
    /// a slope threshold expressed as a fraction of the relief is wanted.
    ///
    /// Dimensionless (rise over run). Computed in `f64` and rounded once, so it
    /// can differ from `gradient(at).length()` — which rounds twice — by an
    /// `f32` ulp. Both are deterministic; pick one per call site and stay with
    /// it rather than mixing them inside a single comparison.
    #[must_use]
    pub fn slope(&self, at: Point) -> f32 {
        narrow(self.slope_f64(f64::from(at.x), f64::from(at.y)))
    }

    /// [`TerrainField::height`] in `f64` — the form [`crate::roads`] should use
    /// when it is accumulating rather than storing.
    #[must_use]
    pub fn height_f64(&self, x: f64, y: f64) -> f64 {
        self.sample(x, y).0
    }

    /// [`TerrainField::gradient`] in `f64`, as `(dx, dy)`. Downhill.
    #[must_use]
    pub fn gradient_f64(&self, x: f64, y: f64) -> (f64, f64) {
        let (_, dx, dy) = self.sample(x, y);
        (dx, dy)
    }

    /// [`TerrainField::slope`] in `f64`.
    #[must_use]
    pub fn slope_f64(&self, x: f64, y: f64) -> f64 {
        let (_, dx, dy) = self.sample(x, y);
        (dx * dx + dy * dy).sqrt()
    }

    /// Height and downhill gradient in one pass — `(height, -∂h/∂x, -∂h/∂y)`.
    ///
    /// The one place the field is actually evaluated. Everything else here is a
    /// projection of this, so there is exactly one summation order to reason
    /// about: noise first, then district biases in sorted-path order.
    #[must_use]
    pub fn sample(&self, x: f64, y: f64) -> (f64, f64, f64) {
        if !x.is_finite() || !y.is_finite() {
            return (0.0, 0.0, 0.0);
        }

        // Base noise. The chain rule brings the frequency out of the sample.
        let (noise, noise_slope_x, noise_slope_y) = fbm2_with_gradient(
            self.seed,
            x * self.frequency,
            y * self.frequency,
            self.octaves,
        );
        let mut height = self.relief * noise;
        let mut uphill_x = self.relief * self.frequency * noise_slope_x;
        let mut uphill_y = self.relief * self.frequency * noise_slope_y;

        // District biases, in the canonical order the sorted `Vec` guarantees.
        // Re-ordering this loop changes the last bit of every height, and
        // therefore the city (PRD §7.4).
        for bias in &self.biases {
            if bias.height == 0.0 {
                continue;
            }
            let (weight, weight_slope_x, weight_slope_y) = bias.weight_and_gradient(x, y);
            if weight == 0.0 {
                continue;
            }
            height += bias.height * weight;
            uphill_x += bias.height * weight_slope_x;
            uphill_y += bias.height * weight_slope_y;
        }

        (height, -uphill_x, -uphill_y)
    }

    /// A stable digest of the whole field, for the determinism suite.
    ///
    /// Hashes the raw `f64` bits of the height and both gradient components at
    /// `resolution × resolution` points spanning `[-extent, extent]`, plus the
    /// field's own parameters. Bit-level, not tolerance-based, on purpose: this
    /// is what PRD §15's M1 gate — "byte-identical layout across two runs and
    /// across two machines" — actually means for a field that is never
    /// serialized, and it is cheap enough to assert on every CI run.
    ///
    /// A change to the seed, the extent, the noise, the octave count, the depth
    /// bias, or any district centre moves it. Pin it with a literal
    /// ([`crate::determinism`], rule 7).
    #[must_use]
    pub fn digest(&self, resolution: u32) -> u64 {
        use crate::determinism::{combine_seeds, fnv1a64};

        let mut digest = fnv1a64(b"polis terrain digest v1");
        digest = combine_seeds(digest, self.seed);
        digest = combine_seeds(digest, self.extent.to_bits());
        digest = combine_seeds(digest, self.frequency.to_bits());
        digest = combine_seeds(digest, self.relief.to_bits());
        digest = combine_seeds(digest, u64::from(self.octaves));
        for bias in &self.biases {
            digest = combine_seeds(digest, fnv1a64(bias.path.as_str().as_bytes()));
            digest = combine_seeds(digest, bias.centre_x.to_bits());
            digest = combine_seeds(digest, bias.centre_y.to_bits());
            digest = combine_seeds(digest, bias.height.to_bits());
            digest = combine_seeds(digest, bias.radius_sq.to_bits());
        }

        if resolution == 0 {
            return digest;
        }
        // A fixed traversal order, and a step that is an exact division of an
        // exact span, so the sample points themselves are reproducible.
        let steps = f64::from(resolution);
        for row in 0..resolution {
            for column in 0..resolution {
                let u = f64::from(column) / steps * 2.0 - 1.0;
                let v = f64::from(row) / steps * 2.0 - 1.0;
                let (height, dx, dy) = self.sample(u * self.extent, v * self.extent);
                digest = combine_seeds(digest, height.to_bits());
                digest = combine_seeds(digest, dx.to_bits());
                digest = combine_seeds(digest, dy.to_bits());
            }
        }
        digest
    }
}

#[cfg(test)]
mod tests {
    // Every float comparison in this module is an exact, bit-level assertion —
    // that is what a determinism suite is for (PRD §7.4, §16). `float_cmp`
    // exists to catch approximate equality written as `==`, which is the
    // opposite of what is happening here; anything that genuinely needs a
    // tolerance in these tests is written with one explicitly.
    #![allow(clippy::float_cmp)]

    use std::process::Command;

    use super::*;
    use crate::determinism::SeededRng;

    /// The fixture every pinned value in this module is taken from.
    ///
    /// Deliberately not `LogicalPath::root().layout_seed()`: a pinned digest
    /// should fail when the noise changes, not when someone renames a fixture.
    const FIXTURE_SEED: u64 = 0x0bad_c0de_dead_beef;
    const FIXTURE_EXTENT: f32 = 1_000.0;
    const DIGEST_RESOLUTION: u32 = 24;

    fn path(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("valid logical path")
    }

    /// The fixture field: a city with a handful of districts at varying depths.
    fn fixture() -> TerrainField {
        let mut field = TerrainField::generate(FIXTURE_SEED, FIXTURE_EXTENT);
        for (district, x, y) in [
            ("src", -120.0, 40.0),
            ("src/auth", 260.0, -310.0),
            ("src/auth/providers/oauth", -430.0, 150.0),
            ("docs", 500.0, 480.0),
            ("crates/polis-layout/src", 90.0, -620.0),
        ] {
            field.bias_district(&path(district), Point::new(x, y));
        }
        field
    }

    // -----------------------------------------------------------------------
    // Determinism — the M1 gate (PRD §15)
    // -----------------------------------------------------------------------

    #[test]
    fn two_runs_in_one_process_are_identical() {
        let first = fixture();
        let second = fixture();
        assert_eq!(
            first.digest(DIGEST_RESOLUTION),
            second.digest(DIGEST_RESOLUTION)
        );

        // Not just the digest: every sample, bit for bit.
        let mut rng = SeededRng::for_seed(4, "terrain determinism");
        for _ in 0..20_000 {
            let x = rng.range_f64(-2_000.0, 2_000.0);
            let y = rng.range_f64(-2_000.0, 2_000.0);
            assert_eq!(
                first.sample(x, y).0.to_bits(),
                second.sample(x, y).0.to_bits()
            );
            assert_eq!(
                first.sample(x, y).1.to_bits(),
                second.sample(x, y).1.to_bits()
            );
        }
    }

    #[test]
    fn digest_is_pinned_to_a_literal_value() {
        // PRD §7.4 / ADR-0029. A failure here means the terrain moved, which
        // means every road, block, lot and building moved. That is the signal,
        // not a nuisance — but it must be a *deliberate* signal, so update this
        // number only together with a note saying what changed and why.
        assert_eq!(fixture().digest(DIGEST_RESOLUTION), 0xe168_0ab1_8ca6_1f7d);
        assert_eq!(
            TerrainField::generate(0, 1.0).digest(8),
            0xfe7a_b8de_0691_8358
        );
    }

    /// Two **fresh processes** must produce the same field (PRD §15, M1).
    ///
    /// The same-process test cannot catch a dependency on anything that is
    /// randomised per process — `RandomState`, ASLR-dependent pointer ordering,
    /// a `HashMap` iteration hidden three layers down. This one re-invokes the
    /// test binary twice and compares what it prints, which is the only way to
    /// see that class of bug from inside a test suite.
    #[test]
    fn two_fresh_processes_are_identical() {
        let first = child_digest();
        let second = child_digest();
        assert_eq!(
            first, second,
            "the field differs between two fresh processes"
        );
        assert_eq!(
            first,
            fixture().digest(DIGEST_RESOLUTION),
            "a child process disagrees with this one"
        );
    }

    /// Runs the ignored test below in a fresh process and reads back its digest.
    fn child_digest() -> u64 {
        let exe = std::env::current_exe().expect("test binary path");
        let output = Command::new(exe)
            .args([
                "--exact",
                "terrain::tests::print_fixture_digest",
                "--ignored",
                "--nocapture",
            ])
            // Pinned, not inherited. Single-threaded is the harness mode whose
            // output interleaving broke the parser below, so the child is always
            // run in it — a parse that only works when the developer happens to
            // launch the suite multi-threaded is a determinism test that stops
            // running the moment CI sets `RUST_TEST_THREADS: 1`.
            .env("RUST_TEST_THREADS", "1")
            .output()
            .expect("re-invoke the test binary");
        assert!(
            output.status.success(),
            "child process failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        // The marker is searched for **anywhere in the stream**, never at the
        // start of a line, and that is not defensive style — a line-anchored
        // parser here is a live bug.
        //
        // The child inherits this process's environment, `RUST_TEST_THREADS`
        // included. In single-threaded mode libtest writes `test <name> ... `
        // with **no trailing newline** before running the test, so the child's
        // first `--nocapture` line arrives as
        // `test terrain::tests::print_fixture_digest ... POLIS_TERRAIN_DIGEST=…`
        // and a `strip_prefix` sees no digest at all. The test then panics on
        // its own output format instead of comparing two processes — so the one
        // assertion that can catch `RandomState` reaching the layout stops
        // running, in exactly the CI job that sets `RUST_TEST_THREADS: 1` to run
        // it. Parse position must not be part of what this test asserts.
        let digest = stdout
            .split_once("POLIS_TERRAIN_DIGEST=")
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .unwrap_or_else(|| panic!("child printed no digest:\n{stdout}"));
        u64::from_str_radix(digest, 16).expect("hex digest")
    }

    /// The child half of [`two_fresh_processes_are_identical`]. Ignored, so it
    /// only ever runs when that test asks for it by name.
    #[test]
    #[ignore = "child process of two_fresh_processes_are_identical"]
    fn print_fixture_digest() {
        println!(
            "POLIS_TERRAIN_DIGEST={:016x}",
            fixture().digest(DIGEST_RESOLUTION)
        );
    }

    #[test]
    fn bias_order_does_not_change_the_field() {
        // The concrete pay-off of rule 2: even a caller that iterates a
        // `HashMap` of districts gets the same terrain.
        let districts = [
            ("src", Point::new(-120.0, 40.0)),
            ("src/auth", Point::new(260.0, -310.0)),
            ("src/auth/providers/oauth", Point::new(-430.0, 150.0)),
            ("docs", Point::new(500.0, 480.0)),
            ("crates/polis-layout/src", Point::new(90.0, -620.0)),
        ];

        let mut forwards = TerrainField::generate(FIXTURE_SEED, FIXTURE_EXTENT);
        for (name, centre) in districts {
            forwards.bias_district(&path(name), centre);
        }
        let mut backwards = TerrainField::generate(FIXTURE_SEED, FIXTURE_EXTENT);
        for (name, centre) in districts.iter().rev() {
            backwards.bias_district(&path(name), *centre);
        }
        let mut hashed = TerrainField::generate(FIXTURE_SEED, FIXTURE_EXTENT);
        let map: std::collections::HashMap<&str, Point> = districts.iter().copied().collect();
        for (name, centre) in map {
            hashed.bias_district(&path(name), centre);
        }

        let expected = forwards.digest(DIGEST_RESOLUTION);
        assert_eq!(backwards.digest(DIGEST_RESOLUTION), expected);
        assert_eq!(hashed.digest(DIGEST_RESOLUTION), expected);
    }

    #[test]
    fn biasing_a_district_twice_replaces_it() {
        // Incremental growth must land where a full regeneration would.
        let mut incremental = fixture();
        incremental.bias_district(&path("docs"), Point::new(500.0, 480.0));
        assert_eq!(incremental.district_count(), fixture().district_count());
        assert_eq!(
            incremental.digest(DIGEST_RESOLUTION),
            fixture().digest(DIGEST_RESOLUTION)
        );

        // And a moved centre is a move, not an addition.
        let mut moved = fixture();
        moved.bias_district(&path("docs"), Point::new(-500.0, 0.0));
        assert_eq!(moved.district_count(), fixture().district_count());
        assert_ne!(
            moved.digest(DIGEST_RESOLUTION),
            fixture().digest(DIGEST_RESOLUTION)
        );
    }

    // -----------------------------------------------------------------------
    // The field itself
    // -----------------------------------------------------------------------

    #[test]
    fn field_is_finite_everywhere_including_far_outside_the_city() {
        let field = fixture();
        let mut rng = SeededRng::for_seed(17, "finite");
        for _ in 0..50_000 {
            let x = rng.range_f64(-50_000.0, 50_000.0);
            let y = rng.range_f64(-50_000.0, 50_000.0);
            let (height, dx, dy) = field.sample(x, y);
            assert!(height.is_finite(), "height {height} at ({x}, {y})");
            assert!(dx.is_finite() && dy.is_finite(), "gradient at ({x}, {y})");
        }
        // The exact lattice corners, where the corner weights hit zero.
        for i in -8..8 {
            for j in -8..8 {
                let x = f64::from(i) / field.frequency;
                let y = f64::from(j) / field.frequency;
                let (height, dx, dy) = field.sample(x, y);
                assert!(height.is_finite() && dx.is_finite() && dy.is_finite());
            }
        }
    }

    #[test]
    fn field_is_total_over_degenerate_input() {
        let field = fixture();
        assert_eq!(field.sample(f64::NAN, 0.0), (0.0, 0.0, 0.0));
        assert_eq!(field.sample(0.0, f64::INFINITY), (0.0, 0.0, 0.0));
        assert!(field.height(Point::new(f32::NAN, 0.0)).is_finite());

        // A degenerate extent degrades to a unit city, never to a NaN field.
        for extent in [0.0, -0.0, f32::NAN, f32::INFINITY] {
            let degenerate = TerrainField::generate(1, extent);
            assert_eq!(degenerate.extent(), 1.0);
            assert!(degenerate.height(Point::new(0.5, 0.5)).is_finite());
        }
        // A negative extent is the same city as its positive twin.
        assert_eq!(
            TerrainField::generate(1, -400.0).digest(8),
            TerrainField::generate(1, 400.0).digest(8)
        );

        // A non-finite district centre is ignored rather than poisoning the field.
        let mut poisoned = fixture();
        poisoned.bias_district(&path("bad"), Point::new(f32::NAN, 0.0));
        assert_eq!(poisoned.district_count(), fixture().district_count());
    }

    #[test]
    fn default_field_is_flat() {
        let flat = TerrainField::default();
        assert!(flat.is_flat());
        assert_eq!(flat.height(Point::new(12.0, -30.0)), 0.0);
        assert_eq!(flat.gradient(Point::new(12.0, -30.0)), Vec2::ZERO);
        assert_eq!(flat.slope(Point::new(12.0, -30.0)), 0.0);
        assert_eq!(flat.octaves(), 0);
        assert!(!fixture().is_flat());
    }

    #[test]
    fn relief_is_a_fraction_of_the_extent_and_the_field_uses_it() {
        let field = TerrainField::generate(FIXTURE_SEED, FIXTURE_EXTENT);
        assert!(
            (f64::from(field.relief()) - RELIEF_FRACTION * f64::from(FIXTURE_EXTENT)).abs() < 1.0
        );

        let mut lowest = f64::MAX;
        let mut highest = f64::MIN;
        let mut rng = SeededRng::for_seed(21, "relief");
        for _ in 0..40_000 {
            let x = rng.range_f64(-1_000.0, 1_000.0);
            let y = rng.range_f64(-1_000.0, 1_000.0);
            let height = field.height_f64(x, y);
            lowest = lowest.min(height);
            highest = highest.max(height);
            assert!(height.abs() <= f64::from(field.relief()) + 1e-9, "{height}");
        }
        // The relief must actually be used, not merely bounded by.
        assert!(
            highest - lowest > f64::from(field.relief()),
            "field is nearly flat"
        );
    }

    #[test]
    fn the_field_is_low_frequency() {
        // PRD §7.2: contours for roads to follow, not wrinkles. Over a step of a
        // hundredth of the city, the ground must not move by more than a small
        // fraction of its relief — otherwise "follow the gradient" produces a
        // wiggle, which is the failure mode the whole section warns about.
        let field = TerrainField::generate(FIXTURE_SEED, FIXTURE_EXTENT);
        let step = f64::from(FIXTURE_EXTENT) / 100.0;
        let mut worst: f64 = 0.0;
        let mut rng = SeededRng::for_seed(31, "low frequency");
        for _ in 0..20_000 {
            let x = rng.range_f64(-1_000.0, 1_000.0);
            let y = rng.range_f64(-1_000.0, 1_000.0);
            worst = worst.max((field.height_f64(x + step, y) - field.height_f64(x, y)).abs());
        }
        assert!(
            worst < f64::from(field.relief()) * 0.5,
            "field is not low-frequency: {worst}"
        );
    }

    // -----------------------------------------------------------------------
    // The gradient — the real deliverable
    // -----------------------------------------------------------------------

    /// The gradient must be the derivative of the height field it is sampled
    /// from, including through the district bias — otherwise roads follow a
    /// contour that is not there.
    ///
    /// Property-style over points drawn from the crate's own generator, so the
    /// sample set is deterministic and a failure is reproducible from its seed.
    #[test]
    fn gradient_is_consistent_with_the_height_field() {
        let field = fixture();
        let step = 1e-3;
        let mut worst: f64 = 0.0;
        let mut rng = SeededRng::for_seed(55, "gradient property");
        for _ in 0..30_000 {
            let x = rng.range_f64(-1_500.0, 1_500.0);
            let y = rng.range_f64(-1_500.0, 1_500.0);
            let (_, downhill_x, downhill_y) = field.sample(x, y);
            // Central difference of the height, negated: the field's gradient is
            // the descent direction.
            let numeric_x =
                -(field.height_f64(x + step, y) - field.height_f64(x - step, y)) / (2.0 * step);
            let numeric_y =
                -(field.height_f64(x, y + step) - field.height_f64(x, y - step)) / (2.0 * step);
            worst = worst
                .max((downhill_x - numeric_x).abs())
                .max((downhill_y - numeric_y).abs());
        }
        assert!(
            worst < 1e-5,
            "gradient disagrees with the height field by {worst}"
        );
    }

    #[test]
    fn gradient_points_downhill() {
        let field = fixture();
        let mut rng = SeededRng::for_seed(77, "downhill");
        let mut checked = 0;
        for _ in 0..5_000 {
            let x = rng.range_f64(-1_200.0, 1_200.0);
            let y = rng.range_f64(-1_200.0, 1_200.0);
            let (height, dx, dy) = field.sample(x, y);
            let slope = (dx * dx + dy * dy).sqrt();
            if slope < 1e-4 {
                continue;
            }
            let step = 1e-2;
            let downhill = field.height_f64(x + dx / slope * step, y + dy / slope * step);
            assert!(downhill <= height, "gradient pointed uphill at ({x}, {y})");
            checked += 1;
        }
        assert!(
            checked > 4_000,
            "too few points had a usable slope: {checked}"
        );
    }

    #[test]
    fn slope_agrees_with_the_gradient_length() {
        let field = fixture();
        let mut rng = SeededRng::for_seed(88, "slope");
        for _ in 0..20_000 {
            let x = rng.range_f64(-1_500.0, 1_500.0);
            let y = rng.range_f64(-1_500.0, 1_500.0);
            let at = Point::new(narrow(x), narrow(y));
            let slope = field.slope(at);
            let length = field.gradient(at).length();
            assert!(slope.is_finite() && slope >= 0.0);
            assert!(
                (slope - length).abs() <= 1e-6 * (1.0 + slope),
                "slope {slope} vs gradient length {length}"
            );
        }
    }

    #[test]
    fn slope_is_dimensionless_and_in_a_usable_range() {
        // The property that lets `GrowthParams::slope_threshold` be a plain
        // number: the slope distribution must not move when the city does.
        let mut small = 0.0;
        let mut large = 0.0;
        for (extent, into) in [(100.0_f32, &mut small), (10_000.0_f32, &mut large)] {
            let field = TerrainField::generate(FIXTURE_SEED, extent);
            let mut peak: f64 = 0.0;
            let mut rng = SeededRng::for_seed(101, "slope scale");
            for _ in 0..20_000 {
                let unit_x = rng.range_f64(-1.0, 1.0);
                let unit_y = rng.range_f64(-1.0, 1.0);
                peak = peak
                    .max(field.slope_f64(unit_x * f64::from(extent), unit_y * f64::from(extent)));
            }
            *into = peak;
        }
        assert!(
            (small - large).abs() < 1e-6,
            "slope depends on extent: {small} vs {large}"
        );
        assert!(
            (0.05..5.0).contains(&small),
            "slope is unusable as a threshold: {small}"
        );
    }

    // -----------------------------------------------------------------------
    // The depth bias
    // -----------------------------------------------------------------------

    #[test]
    fn deeper_directories_are_higher_ground() {
        assert_eq!(depth_bias_fraction(0), 0.0);
        let mut previous = 0.0;
        for depth in 1..20 {
            let bias = depth_bias_fraction(depth);
            assert!(
                bias > previous,
                "depth {depth} is not higher than {}",
                depth - 1
            );
            assert!(bias < DEPTH_RELIEF_FRACTION, "depth bias must saturate");
            previous = bias;
        }

        // And it shows up in the field: the same centre, biased at two depths.
        let centre = Point::new(0.0, 0.0);
        let mut shallow = TerrainField::generate(FIXTURE_SEED, FIXTURE_EXTENT);
        shallow.bias_district(&path("src"), centre);
        let mut deep = TerrainField::generate(FIXTURE_SEED, FIXTURE_EXTENT);
        deep.bias_district(&path("src/auth/providers/oauth/google"), centre);
        assert!(deep.height(centre) > shallow.height(centre));

        // The root district contributes nothing — PRD §8's civic square is not a
        // hill.
        let mut rooted = TerrainField::generate(FIXTURE_SEED, FIXTURE_EXTENT);
        rooted.bias_district(&LogicalPath::root(), centre);
        let bare = TerrainField::generate(FIXTURE_SEED, FIXTURE_EXTENT);
        assert_eq!(rooted.height(centre), bare.height(centre));
    }

    #[test]
    fn a_district_bias_is_local_and_smooth() {
        let centre = Point::new(0.0, 0.0);
        let bare = TerrainField::generate(FIXTURE_SEED, FIXTURE_EXTENT);
        let mut biased = bare.clone();
        biased.bias_district(&path("a/b/c"), centre);

        // Inside the radius it lifts the ground.
        assert!(biased.height(centre) > bare.height(centre));
        // Outside it changes nothing at all — bit for bit, so the falloff really
        // does reach zero rather than merely becoming small.
        let radius = DISTRICT_RADIUS_FRACTION * f64::from(FIXTURE_EXTENT);
        let outside = radius * 1.01;
        assert_eq!(
            biased.height_f64(outside, 0.0).to_bits(),
            bare.height_f64(outside, 0.0).to_bits()
        );
        assert_eq!(
            biased.gradient_f64(0.0, outside),
            bare.gradient_f64(0.0, outside)
        );

        // And the rim is smooth: no crease for a road to follow, which would
        // draw the district outline in streets. Measured on what the bias *adds*
        // to the gradient, since the noise underneath differs between any two
        // sample points.
        let added_slope = |distance: f64| {
            let (_, biased_x, biased_y) = biased.sample(distance, 0.0);
            let (_, bare_x, bare_y) = bare.sample(distance, 0.0);
            let (dx, dy) = (biased_x - bare_x, biased_y - bare_y);
            (dx * dx + dy * dy).sqrt()
        };
        // The bias's own slope peaks around `0.45 R` and must fade to nothing at
        // the rim rather than stopping at a step.
        let peak = added_slope(radius * 0.45);
        assert!(peak > 0.0, "the district bias adds no slope at all");
        assert!(
            added_slope(radius * 0.99) < peak * 0.01,
            "district rim has a crease: {} against a peak of {peak}",
            added_slope(radius * 0.99)
        );
        assert_eq!(added_slope(radius * 1.01), 0.0);
    }

    #[test]
    fn digest_moves_when_anything_layout_visible_moves() {
        let base = fixture().digest(DIGEST_RESOLUTION);
        assert_ne!(
            base,
            TerrainField::generate(FIXTURE_SEED + 1, FIXTURE_EXTENT).digest(DIGEST_RESOLUTION)
        );
        assert_ne!(
            base,
            TerrainField::generate(FIXTURE_SEED, FIXTURE_EXTENT * 2.0).digest(DIGEST_RESOLUTION)
        );

        let mut extra = fixture();
        extra.bias_district(&path("tests/fixtures"), Point::new(10.0, 10.0));
        assert_ne!(base, extra.digest(DIGEST_RESOLUTION));

        // Resolution 0 hashes the parameters only, so it still separates two
        // different fields without sampling anything.
        assert_ne!(
            fixture().digest(0),
            TerrainField::generate(FIXTURE_SEED, 1.0).digest(0)
        );
    }
}
