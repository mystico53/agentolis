//! Determinism (PRD §7.4) — the hard requirement everything else rests on.
//!
//! > **Every random draw is seeded from a hash of the logical path.** Never from
//! > wall clock, never from a global RNG, never from iteration order of a
//! > `HashMap`.
//!
//! > Use `BTreeMap` wherever iteration order can affect layout. This is a common
//! > source of nondeterminism and it will be subtle when it bites.
//!
//! Everything in this module is **written out** rather than pulled from a crate,
//! for the same reason [`polis_events::LogicalPath::layout_seed`] does not use
//! `DefaultHasher`: a dependency may change its algorithm in a patch release and
//! silently reshuffle every city in existence. PRD §7.4 promises "the same repo
//! produces the same city on every launch and on every machine", which includes
//! machines on a different toolchain and a different lockfile (ADR-0029).
//!
//! That applies to the noise as much as to the generator. `polis-layout` has
//! **no `noise` dependency**: no noise crate promises output stability across
//! releases, and PRD §7.2 makes the terrain field the thing every road contour
//! follows, so a changed noise function is a completely different city. Simplex
//! is implemented here instead (ADR-0050).
//!
//! # The seven rules
//!
//! Every later layout module is bound by these. They are short on purpose.
//!
//! 1. **Seed from the logical path.** [`SeededRng::for_path`], never
//!    [`std::hash::DefaultHasher`], never `rand`, never the clock. Each draw
//!    site passes its own `purpose` tag so a new draw cannot shift every later
//!    one.
//! 2. **Iterate in a canonical order.** `BTreeMap`, `BTreeSet`, or a `Vec` whose
//!    order is derived from sorted input. Never a `HashMap`, never a `HashSet`,
//!    never `read_dir`. [`debug_assert_canonical_order`] catches the mistake at
//!    the boundary where it would otherwise become a silently different city.
//! 3. **Do the arithmetic in `f64`, in a fixed association order.** See
//!    *Float discipline* below.
//! 4. **Never call `sin`, `cos`, `atan2`, `exp`, `ln` or `powf` on a value that
//!    reaches the layout.** Use [`det_sin_cos`], [`det_from_angle`],
//!    [`det_angle`] — or, better, an algorithm that needs none of them, which is
//!    why the noise in this module contains no transcendental function at all.
//! 5. **Never `mul_add`.** A fused multiply-add rounds once and the unfused pair
//!    rounds twice; whether LLVM emits one is a target and opt-level decision,
//!    so the same source can produce two cities. Write `a * b + c`.
//! 6. **Quantise at the serialization boundary, never inside the algorithm.**
//!    [`quantize`] on the way into a snapshot. Quantising mid-algorithm makes
//!    the layout depend on rounding at every step, which is worse than the
//!    problem it solves.
//! 7. **Pin every new generator with literal expected values.** If its output
//!    reaches the layout — a hash, a sampler, a relaxation — it gets a test with
//!    numbers written into it, exactly as `layout_seed` and [`simplex2`] have.
//!    When such a test fails, every golden layout file is invalidated **on
//!    purpose**. That is the signal, not a nuisance (ADR-0029, ADR-0050).
//!
//! # Float discipline
//!
//! The crate's shared geometry ([`Point`], [`Vec2`]) is `f32`, because it is
//! handed to the renderer and stored in [`crate::CityLayout`]. **The arithmetic
//! that produces those values is `f64`**, with exactly one rounding, at the end,
//! through [`narrow`]. Two reasons:
//!
//! * `f32` has 24 bits of mantissa. A city of extent 10 000 has an `f32` spacing
//!   of about 0.001 near its edge, which is [`QUANTUM`] — the accumulation error
//!   of a road-growth loop would be the same size as the quantisation step it is
//!   supposed to survive.
//! * Rounding once at the end is a single, documented, deterministic operation.
//!   `f64 -> f32` is correctly rounded on every IEEE-754 target, so two machines
//!   that agree in `f64` agree in `f32`.
//!
//! What is safe to rely on, and why this module is careful anyway:
//!
//! * `+ - * /` and `sqrt` are **correctly rounded** by IEEE-754 and reproducible
//!   across x86-64, aarch64 and every target Polis will see. Rust does not
//!   enable fast-math and does not contract `a * b + c` into an FMA on its own,
//!   so a fixed source expression is a fixed sequence of roundings.
//! * Re-associating a sum is **not** free: `(a + b) + c` and `a + (b + c)` can
//!   differ in the last bit. Sums here are written in one order and iterated in
//!   one order, which is rule 2 again from the other direction.
//! * Transcendental functions (`sin`, `cos`, `exp`, `powf`) are **not** covered
//!   by IEEE-754's correct-rounding requirement. Two libms may differ in the
//!   last ulp for the same input, and that is the exact failure PRD §16's
//!   two-OS comparison exists to catch. Rules 4 and 6 are the defence.
//! * 32-bit x86 (x87, 80-bit intermediates) is **not supported** for the
//!   byte-identical guarantee. `x86_64`, `aarch64` and any other SSE2-class
//!   target is.
//!
//! # Pin the outputs with literal expected values
//!
//! [`SeededRng`], [`simplex2`], [`fbm2`], the direction table and
//! [`crate::terrain::TerrainField::digest`] are each pinned by a test with
//! literal numbers in it, exactly as `layout_seed` is, plus a test that fails if
//! anyone swaps the written-out hash for `DefaultHasher`.
//!
//! Two things that are checked rather than assumed, and are worth re-running
//! whenever this module changes:
//!
//! * **`cargo test -p polis-layout --release` must pass the same pins as the
//!   debug run.** It does today. That is the practical evidence for rule 5:
//!   `opt-level = 0` and `opt-level = 3` produce identical bits, so LLVM is not
//!   contracting or re-associating anything here.
//! * **`terrain::tests::two_fresh_processes_are_identical`** re-invokes the test
//!   binary and compares digests, which is the only way a test suite can see a
//!   dependency on something randomised per process (`RandomState`, ASLR-ordered
//!   pointers, an environment variable read three layers down).
//!
//! Neither substitutes for PRD §15's real M1 gate — the same golden file on two
//! *machines* — but both fail on this machine, today, for the mistakes that gate
//! would otherwise catch a week later.

use std::cmp::Ordering;
use std::fmt::Debug;

use polis_events::LogicalPath;

use crate::{Point, Vec2};

// ---------------------------------------------------------------------------
// Hashing — written out, because a hash that moves moves the city
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit offset basis. Same constant `LogicalPath::layout_seed` uses.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The 64-bit FNV-1a hash of `bytes`, written out (ADR-0029).
///
/// This is the crate's only hash. It is deliberately **not**
/// [`std::hash::DefaultHasher`]: `SipHash`'s output is explicitly not guaranteed
/// stable across Rust releases, so a toolchain upgrade would silently reshuffle
/// every city — the failure would present months later as "the golden file
/// changed" with no cause in the diff.
///
/// FNV-1a is a weak hash in the adversarial sense and a perfectly good one here:
/// nothing about a city layout is attacker-controlled, and [`mix64`] supplies
/// the avalanche that FNV lacks.
///
/// Pinned by `hash_is_fnv1a_and_not_default_hasher`.
#[inline]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// [`fnv1a64`] over a string's UTF-8 bytes.
///
/// Case is **not** folded here. `LogicalPath::layout_seed` folds ASCII case
/// because two spellings of a path are the same file; a purpose tag is a literal
/// written in this crate's source and is compared exactly.
#[inline]
pub fn fnv1a64_str(text: &str) -> u64 {
    fnv1a64(text.as_bytes())
}

/// The `SplitMix64` finalizer — a written-out bijective avalanche mixer.
///
/// Every bit of the output depends on every bit of the input, which is what
/// turns `FNV-1a`'s poor low-bit diffusion into a usable seed. Bijective, so it
/// cannot collide two distinct seeds into one.
///
/// The three constants are the published `SplitMix64` ones and are written out
/// here so that no dependency, and no future edit to a dependency, can move
/// them.
#[inline]
pub fn mix64(seed: u64) -> u64 {
    let mut z = seed;
    z ^= z >> 30;
    z = z.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z ^= z >> 27;
    z = z.wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Salt folded into [`combine_seeds`].
///
/// Load-bearing: [`mix64`] maps `0` to `0` (a property of the `SplitMix64`
/// finalizer), so without a salt `combine_seeds(0, 0)` would be `0` and a
/// zero-seeded field would sit on a degenerate stream.
const COMBINE_SALT: u64 = 0x2545_f491_4f6c_dd1d;

/// Combines two seeds into one, order-dependently.
///
/// `combine_seeds(a, b) != combine_seeds(b, a)`, deliberately: a `(district,
/// index)` pair and an `(index, district)` pair are different draw sites and
/// must not collide. Chained, it is the crate's general "derive a seed from
/// these things, in this order" primitive.
#[inline]
pub fn combine_seeds(left: u64, right: u64) -> u64 {
    mix64(mix64(left ^ COMBINE_SALT) ^ right.wrapping_mul(FNV_PRIME))
}

/// The seed for one draw site: a logical path and a purpose tag (PRD §7.4).
///
/// This is the function the whole product's spatial memory rests on. It is a
/// pure function of the path's bytes and the tag's bytes — nothing else, ever.
#[inline]
pub fn seed_for_path(path: &LogicalPath, purpose: &str) -> u64 {
    combine_seeds(path.layout_seed(), fnv1a64_str(purpose))
}

// ---------------------------------------------------------------------------
// The generator
// ---------------------------------------------------------------------------

/// The `SplitMix64` step constant (the odd 64-bit approximation of the golden
/// ratio). Written out; see [`mix64`].
const GOLDEN_GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

/// A seeded, path-local pseudo-random generator.
///
/// One is constructed per draw site from `(path, purpose)`, so adding a new draw
/// somewhere in the pipeline cannot shift the numbers every later draw sees —
/// which a single shared stream would.
///
/// The algorithm is `SplitMix64`, written out (ADR-0029): a counter advanced by
/// [`GOLDEN_GAMMA`] and passed through [`mix64`]. It is small enough to audit in
/// one screen, has no state beyond a `u64`, and passes `BigCrush` — which is far
/// more than a city needs from a generator whose only job is to look arbitrary.
///
/// Not `Copy`, on purpose: two copies of a stream silently produce the same
/// numbers twice, and a draw site that wanted a fresh stream should say so with
/// [`SeededRng::sub`].
#[derive(Debug, Clone)]
pub struct SeededRng {
    state: u64,
}

impl SeededRng {
    /// Seeds from a logical path and a purpose tag.
    ///
    /// The purpose tag is what keeps draw sites independent: `for_path(p,
    /// "rotation")` and `for_path(p, "roof")` are uncorrelated streams, so a
    /// later change to roof selection cannot rotate every building in the city.
    #[must_use]
    pub fn for_path(path: &LogicalPath, purpose: &str) -> Self {
        Self::from_state(seed_for_path(path, purpose))
    }

    /// Seeds from a raw value and a purpose tag, for draws that belong to a
    /// block or a district rather than to a file.
    #[must_use]
    pub fn for_seed(seed: u64, purpose: &str) -> Self {
        Self::from_state(combine_seeds(seed, fnv1a64_str(purpose)))
    }

    /// Seeds from a path, a purpose tag and an index — one stream per element of
    /// a collection.
    ///
    /// Use this rather than drawing `n` values from one stream when the
    /// collection can grow: inserting an element at position 3 must not change
    /// what elements 4 and later were given.
    #[must_use]
    pub fn for_path_indexed(path: &LogicalPath, purpose: &str, index: u64) -> Self {
        Self::from_state(combine_seeds(seed_for_path(path, purpose), index))
    }

    /// A child stream, without advancing this one.
    ///
    /// The sub-stream for a given `purpose` is a pure function of this stream's
    /// current state, so `rng.sub("a")` and `rng.sub("b")` are independent of
    /// each other and of everything `rng` produces afterwards.
    #[must_use]
    pub fn sub(&self, purpose: &str) -> Self {
        Self::from_state(combine_seeds(self.state, fnv1a64_str(purpose)))
    }

    /// The generator's current state — for a test that wants to prove two
    /// streams are distinct, not for layout code.
    #[must_use]
    pub fn state(&self) -> u64 {
        self.state
    }

    fn from_state(seed: u64) -> Self {
        // One mix at construction, so that two seeds differing in one bit do not
        // produce two streams whose first outputs are close.
        Self { state: mix64(seed) }
    }

    /// Next raw value.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GOLDEN_GAMMA);
        mix64(self.state)
    }

    /// Uniform in `[0, 1)`, with 53 bits of resolution.
    ///
    /// The `f64` form is the primary one: layout arithmetic is `f64` (see the
    /// module docs), and `1 / 2^53` is exact, so the multiply is a single
    /// exactly-rounded operation on every target.
    pub fn next_f64(&mut self) -> f64 {
        // 53 bits, the f64 mantissa, so every product below is exact.
        const SCALE: f64 = 1.0 / 9_007_199_254_740_992.0; // 2^-53
        #[allow(clippy::cast_precision_loss)] // 53 bits into a 53-bit mantissa
        let bits = (self.next_u64() >> 11) as f64;
        bits * SCALE
    }

    /// Uniform in `[0, 1)`, with 24 bits of resolution.
    ///
    /// Deliberately **not** `next_f64() as f32`: an `f64` just below 1 rounds up
    /// to exactly `1.0` in `f32`, which silently breaks the half-open range and
    /// hands a caller a `1.0` where it proved it could never get one. Drawing 24
    /// bits directly makes the bound structural.
    pub fn next_f32(&mut self) -> f32 {
        const SCALE: f32 = 1.0 / 16_777_216.0; // 2^-24
        #[allow(clippy::cast_precision_loss)] // 24 bits into a 24-bit mantissa
        let bits = (self.next_u64() >> 40) as f32;
        bits * SCALE
    }

    /// Uniform in `[low, high)`. Returns `low` when the range is empty or
    /// inverted, rather than producing a `NaN` that deletes a building.
    pub fn range_f32(&mut self, low: f32, high: f32) -> f32 {
        narrow(self.range_f64(f64::from(low), f64::from(high)))
    }

    /// Uniform in `[low, high)` in `f64`. Returns `low` when the range is empty,
    /// inverted, or not finite.
    pub fn range_f64(&mut self, low: f64, high: f64) -> f64 {
        // The finiteness checks come first so that the ordering test below is a
        // plain comparison: a `NaN` bound has already been turned away.
        if !low.is_finite() || !high.is_finite() || high <= low {
            return low;
        }
        let value = low + (high - low) * self.next_f64();
        // `next_f64() < 1` guarantees this mathematically, but the multiply and
        // the add each round, so the boundary is checked rather than assumed.
        if value < high {
            value
        } else {
            low
        }
    }

    /// Uniform in `[0, n)`. Returns 0 for `n == 0`.
    ///
    /// Rejects the modulo bias rather than taking `next_u64() % n`: the bias is
    /// invisible at `n = 3` (roof forms) and obvious at large `n`, and a
    /// selection rule that is subtly non-uniform is worse than one that is
    /// obviously wrong.
    ///
    /// Rejection is from the **bottom** of the range — values below
    /// `2^64 mod n` are redrawn — so the loop is a fixed, written-out rule and
    /// not a library's choice of strategy. It terminates with probability 1 and
    /// in practice on the first draw: the rejection zone is under `n / 2^64` of
    /// the space.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        // 2^64 mod n, computed without a 128-bit type.
        let threshold = (u64::MAX % n).wrapping_add(1) % n;
        loop {
            let draw = self.next_u64();
            if draw >= threshold {
                return draw % n;
            }
        }
    }

    /// Picks one element, deterministically. `None` for an empty slice.
    pub fn choose<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        let count = u64::try_from(items.len()).ok()?;
        if count == 0 {
            return None;
        }
        let index = usize::try_from(self.below(count)).ok()?;
        items.get(index)
    }

    /// A unit vector with a deterministic direction.
    ///
    /// Drawn from a table of [`UNIT_DIRECTIONS`] evenly spaced directions rather
    /// than from `Vec2::from_angle`, because that would call `sin` and `cos` on
    /// a value that reaches the layout (rule 4). The table is fine enough that
    /// nothing in the city can tell: 256 directions is 1.4° apart, well under
    /// the ±4° building rotation PRD §7.3 asks for.
    pub fn unit_vector(&mut self) -> Vec2 {
        let index = self.below(u64::from(UNIT_DIRECTIONS));
        #[allow(clippy::cast_precision_loss)] // index < 256
        let turns = index as f64 / f64::from(UNIT_DIRECTIONS);
        let (sin, cos) = det_sin_cos(turns * TAU);
        Vec2::new(narrow(cos), narrow(sin))
    }
}

/// Directions in [`SeededRng::unit_vector`]'s table.
pub const UNIT_DIRECTIONS: u32 = 256;

// ---------------------------------------------------------------------------
// Float discipline
// ---------------------------------------------------------------------------

/// A full turn in radians.
///
/// Re-exported from `std` so that layout code turning a fraction of a turn into
/// an angle has one spelling to reach for, next to the helpers that make the
/// angle safe to use.
pub const TAU: f64 = std::f64::consts::TAU;

/// The single, documented `f64 -> f32` rounding (see the module docs).
///
/// Every value that leaves this crate's `f64` arithmetic for its `f32` geometry
/// goes through here, so there is exactly one place to look when asking where a
/// coordinate lost precision. `f64 -> f32` is correctly rounded on every
/// IEEE-754 target, so this operation is identical on two machines that agree in
/// `f64`.
///
/// Non-finite values pass through unchanged: they are a bug upstream, and
/// [`quantize`] is where they are contained, at the serialization boundary.
#[inline]
#[allow(clippy::cast_possible_truncation)] // the whole point of the function
pub fn narrow(value: f64) -> f32 {
    value as f32
}

/// Coordinate quantisation step, in city-space units.
///
/// One thousandth of a unit: far below anything visible at any zoom, far above
/// the `f32` accumulation differences between two targets.
pub const QUANTUM: f32 = 0.001;

/// `1 / QUANTUM`, exactly representable, so [`quantize`] is a multiply by an
/// exact power of ten and a divide by the same one.
const QUANTUM_SCALE: f64 = 1000.0;

/// Rounds a coordinate to the layout grid before it is serialised.
///
/// Floating-point accumulation differs between targets. PRD §16 compares golden
/// layout files across two operating systems byte for byte, so every coordinate
/// that reaches the snapshot is quantised first — otherwise the determinism test
/// fails on a last-bit difference no human could act on.
///
/// Total, by design:
///
/// * `-0.0` becomes `0.0`. The two serialize differently (`-0.0` and `0.0`) and
///   compare equal, which is the most annoying possible way for a golden file to
///   fail.
/// * A non-finite input becomes `0.0`. `serde_json` writes `NaN` and `inf` as
///   `null`, which round-trips to a deserialization error and turns a bad
///   coordinate into an unreadable golden file. Call [`debug_assert_finite`]
///   first if you want the loud failure — `city::snapshot` should.
#[must_use]
pub fn quantize(value: f32) -> f32 {
    narrow(quantize_f64(f64::from(value)))
}

/// [`quantize`] in `f64`, for a value that has not yet reached the geometry.
#[must_use]
pub fn quantize_f64(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    snap_to_grid(value, QUANTUM_SCALE)
}

/// [`quantize`] applied to both coordinates.
#[must_use]
pub fn quantize_point(p: Point) -> Point {
    Point::new(quantize(p.x), quantize(p.y))
}

/// [`quantize`] applied to both components.
#[must_use]
pub fn quantize_vec2(v: Vec2) -> Vec2 {
    Vec2::new(quantize(v.x), quantize(v.y))
}

/// Quantisation step for the output of a transcendental function.
///
/// Coarser than [`QUANTUM`] by four orders of magnitude relative to a unit
/// value, and still far finer than anything renderable: a sine quantised here is
/// accurate to about 6·10⁻⁶ of a degree of arc.
pub const TRIG_QUANTUM: f64 = 1e-7;

/// `1 / TRIG_QUANTUM`, exactly representable. See [`snap_to_grid`].
const TRIG_SCALE: f64 = 1e7;

/// `sin` and `cos` together, quantised so two platforms agree (rule 4).
///
/// `sin` and `cos` are not required by IEEE-754 to be correctly rounded, and
/// two libms legitimately differ in the last ulp for the same input. That
/// difference is invisible on screen and fatal to a byte comparison, so the
/// result is snapped to a [`TRIG_QUANTUM`] grid.
///
/// # This is a mitigation, not a proof
///
/// Two results that differ by one ulp round to the same grid point unless the
/// exact value sits within an ulp of a grid *midpoint*, which happens for on the
/// order of one input in 10⁹. The real defence is to need no trigonometry at
/// all — which is why the noise in this module has none, and why
/// [`SeededRng::unit_vector`] draws from a table. Reach for this only when an
/// angle genuinely is the input, and keep the number of calls small.
#[must_use]
pub fn det_sin_cos(radians: f64) -> (f64, f64) {
    let (sin, cos) = radians.sin_cos();
    (snap_to_grid(sin, TRIG_SCALE), snap_to_grid(cos, TRIG_SCALE))
}

/// A unit vector at an angle — the deterministic replacement for
/// `Vec2::from_angle` in layout code (rule 4).
#[must_use]
pub fn det_from_angle(radians: f32) -> Vec2 {
    let (sin, cos) = det_sin_cos(f64::from(radians));
    Vec2::new(narrow(cos), narrow(sin))
}

/// A vector's angle in radians — the deterministic replacement for
/// `Vec2::angle` in layout code (rule 4).
///
/// Returns 0 for the zero vector, where `atan2` is free to return anything.
#[must_use]
pub fn det_angle(v: Vec2) -> f32 {
    if v.x == 0.0 && v.y == 0.0 {
        return 0.0;
    }
    let angle = f64::from(v.y).atan2(f64::from(v.x));
    narrow(snap_to_grid(angle, TRIG_SCALE))
}

/// Rounds `value` onto a grid of `1 / scale`.
///
/// `scale` is passed as the reciprocal rather than the step so that both callers
/// can hand over an exactly representable power of ten — `1 / 0.001` computed at
/// runtime is not exactly `1000`, and a quantiser that is itself inexact defeats
/// the point.
///
/// Non-finite input becomes `0.0`; see [`quantize`].
fn snap_to_grid(value: f64, scale: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    let rounded = (value * scale).round() / scale;
    // `-0.0 + 0.0` is `+0.0` under round-to-nearest, and `x + 0.0` is `x` for
    // every other finite `x`. LLVM cannot fold this away without fast-math,
    // precisely because of the negative-zero case. This is the normalisation.
    rounded + 0.0
}

/// Debug-only guard that a value reaching the layout is finite.
///
/// Call it where a `NaN` would first become visible — the end of a subdivision,
/// the start of a serialization — not in an inner loop.
///
/// # Panics
///
/// In a debug build, if `value` is `NaN` or infinite.
#[inline]
pub fn debug_assert_finite(value: f32, what: &str) {
    debug_assert!(
        value.is_finite(),
        "non-finite value reached the layout at {what}: {value}. \
         A NaN here becomes `null` in the golden file (PRD §16); find the \
         division by zero or the degenerate normalisation that produced it."
    );
}

// ---------------------------------------------------------------------------
// Ordering — rule 2
// ---------------------------------------------------------------------------

/// Sorts by an `f32` key, deterministically and stably.
///
/// Two reasons not to write `sort_by(|a, b| key(a).partial_cmp(&key(b)).unwrap())`
/// in five modules:
///
/// * `partial_cmp` returns `None` for `NaN`, so the idiom panics on the one
///   input that most needs to be survivable. [`f32::total_cmp`] is a total
///   order over every bit pattern, including both zeros and every `NaN`.
/// * `sort_unstable_by` is not stable, so equal keys come out in an order that
///   depends on the sort's internal pivots. Ties here keep input order — which
///   means **the input order must itself be canonical** (rule 2); this helper
///   preserves determinism, it cannot create it.
pub fn sort_by_f32_key<T, K>(items: &mut [T], key: K)
where
    K: Fn(&T) -> f32,
{
    items.sort_by(|a, b| key(a).total_cmp(&key(b)));
}

/// [`sort_by_f32_key`] for an `f64` key.
pub fn sort_by_f64_key<T, K>(items: &mut [T], key: K)
where
    K: Fn(&T) -> f64,
{
    items.sort_by(|a, b| key(a).total_cmp(&key(b)));
}

/// Sorts and de-duplicates into a canonical order.
///
/// The escape hatch for code that has to consume something unordered — a
/// `HashSet` from another crate, a `read_dir`, a set union. Launder it through
/// here **before** it reaches anything the layout can see, and rule 2 holds
/// again.
pub fn canonical_order<T: Ord>(items: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut out: Vec<T> = items.into_iter().collect();
    out.sort();
    out.dedup();
    out
}

/// Debug-only guard that a sequence is in canonical (non-decreasing) order.
///
/// This is the assertion that catches `HashMap` iteration reaching layout
/// output. A `HashMap` with more than a handful of keys iterates in an order
/// that is randomised per process by `RandomState`, so it fails this check
/// essentially every time — on the first run, in the developer's own test, not
/// six months later on a colleague's machine.
///
/// Cheap enough (one comparison per element, debug builds only) to call at every
/// boundary where an ordered collection becomes a `Vec`.
///
/// Takes anything iterable, so a caller with a `Vec<Thing>` can check the key it
/// actually sorted by — `field.items.iter().map(|t| &t.path)` — without
/// allocating a second collection to check the first one.
///
/// # Panics
///
/// In a debug build, if `items` is not sorted.
pub fn debug_assert_canonical_order<T: Ord + Debug>(
    items: impl IntoIterator<Item = T>,
    what: &str,
) {
    if !cfg!(debug_assertions) {
        return;
    }
    let mut previous: Option<T> = None;
    for item in items {
        if let Some(left) = previous {
            assert!(
                left.cmp(&item) != Ordering::Greater,
                "{what} is not in canonical order: {left:?} precedes {item:?}. \
                 PRD §7.4 — this is what a `HashMap`/`HashSet` iteration looks \
                 like when it reaches the layout. Collect into a `BTreeMap`, or \
                 launder it through `determinism::canonical_order`."
            );
        }
        previous = Some(item);
    }
}

// ---------------------------------------------------------------------------
// Noise — 2-D simplex, written out (ADR-0050)
// ---------------------------------------------------------------------------

/// Skew factor into the simplex lattice: `(sqrt(3) - 1) / 2`.
///
/// Written as a literal rather than computed from `f64::sqrt(3.0)`: `sqrt` is
/// correctly rounded and would give the same bits, but a constant that is
/// visible in the source is a constant nobody can accidentally change the
/// derivation of.
const F2: f64 = 0.366_025_403_784_438_6;

/// Unskew factor out of the simplex lattice: `(3 - sqrt(3)) / 6`.
const G2: f64 = 0.211_324_865_405_187_13;

/// `sqrt(2) / 2` — the diagonal components of [`GRAD2`].
///
/// Spelled out rather than taken from `std::f64::consts::FRAC_1_SQRT_2`, which
/// is the same value. The point of ADR-0050 is that every number the terrain
/// field is built from is visible in this file; a reader checking why two
/// machines disagree should not have to go and look one up.
#[allow(clippy::approx_constant)] // deliberate: see above
const SQRT2_OVER_2: f64 = 0.707_106_781_186_547_6;

/// Eight evenly spaced unit gradients.
///
/// Evenly spaced and **all of unit length**, unlike the classic
/// `{-1, 0, 1}²` table whose diagonals are `sqrt(2)` long: mixing lengths biases
/// the field along the axes, and an axis-biased terrain gives axis-biased roads,
/// which is the grid PRD §7 exists to avoid.
const GRAD2: [(f64, f64); 8] = [
    (1.0, 0.0),
    (SQRT2_OVER_2, SQRT2_OVER_2),
    (0.0, 1.0),
    (-SQRT2_OVER_2, SQRT2_OVER_2),
    (-1.0, 0.0),
    (-SQRT2_OVER_2, -SQRT2_OVER_2),
    (0.0, -1.0),
    (SQRT2_OVER_2, -SQRT2_OVER_2),
];

/// Salt mixed into the corner hash, so that a seed shared with
/// [`SeededRng`] does not make the two correlated.
const NOISE_SALT: u64 = 0x5ed1_7e5b_a9e7_1c03;

/// Scales the raw simplex sum into `[-1, 1]`.
///
/// Measured, not inherited: the classic `70.0` belongs to the classic
/// `{-1, 0, 1}²` gradient table, and [`GRAD2`] is a different set. A grid scan
/// followed by a hill climb puts the extreme of the raw three-corner sum at
/// `0.010080204702811`, so `1 / peak` is `99.2043`. `99.0` sits just inside
/// that: the field uses 99.8 % of its range and
/// `noise_stays_in_range_and_is_finite` asserts both halves of the property —
/// that nothing exceeds `1`, and that the peak gets close enough to `1` that
/// the clamp is a formality rather than a flat top nobody noticed.
const NOISE_SCALE: f64 = 99.0;

/// Radial support of one simplex corner. `0.5` is the squared distance from a
/// simplex's centre to its vertices, so the corner contributions meet exactly at
/// zero and the field is smooth (C³) across every cell boundary.
const CORNER_SUPPORT: f64 = 0.5;

/// Hardest cap on octaves, so a caller cannot turn a typo into a hang.
///
/// PRD §7.2's whole budget is two or three; see [`fbm2`].
pub const MAX_OCTAVES: u32 = 8;

/// Octaves used for the terrain field. Fixed, because it is layout-visible.
pub const TERRAIN_OCTAVES: u32 = 3;

/// The gradient index for a lattice corner — the written-out replacement for a
/// permutation table.
///
/// The skeleton for this module described a seeded 256-entry permutation table,
/// the classic construction. A hash is strictly better here and is what is
/// implemented: a table has to be built (256 shuffles per call, or a cache that
/// becomes shared mutable state on the hot path of road growth), and it repeats
/// every 256 lattice cells, which is a visible tiling artifact in a city whose
/// extent is not known in advance. [`mix64`] gives the same avalanche with no
/// table, no state, and no period.
#[inline]
fn corner_gradient(seed: u64, i: i64, j: i64) -> (f64, f64) {
    #[allow(clippy::cast_sign_loss)] // reinterpreting the bits, not converting
    let (i_bits, j_bits) = (i as u64, j as u64);
    let hash = mix64(mix64(seed ^ NOISE_SALT ^ i_bits) ^ j_bits.rotate_left(32));
    GRAD2[(hash >> 61) as usize & 7]
}

/// One corner's contribution and its two partial derivatives.
///
/// `dx`/`dy` are the offsets from the corner to the sample point. Within a
/// simplex the corner is constant, so `d(dx)/dx == 1` exactly and the chain rule
/// is the whole derivation:
///
/// ```text
/// t = 0.5 - dx² - dy²          n  = t⁴ · (g · d)
/// ∂t/∂x = -2·dx                ∂n/∂x = -8·t³·dx·(g · d) + t⁴·g.x
/// ```
#[inline]
fn corner_contribution(seed: u64, i: i64, j: i64, dx: f64, dy: f64) -> (f64, f64, f64) {
    let t = CORNER_SUPPORT - dx * dx - dy * dy;
    if t <= 0.0 {
        return (0.0, 0.0, 0.0);
    }
    let (gx, gy) = corner_gradient(seed, i, j);
    let dot = gx * dx + gy * dy;
    let t2 = t * t;
    let t3 = t2 * t;
    let t4 = t2 * t2;
    let common = -8.0 * t3 * dot;
    (t4 * dot, common * dx + t4 * gx, common * dy + t4 * gy)
}

/// 2-D simplex noise in `[-1, 1]` with its analytic gradient, in `f64`.
///
/// The primitive every other noise function here is built from. Returns
/// `(value, d/dx, d/dy)`; computing the gradient alongside the value costs
/// almost nothing (the corner weights are already in registers) and is exact,
/// where a finite difference would be a second tuning parameter baked into the
/// layout.
///
/// Contains **no transcendental function** — only `+`, `-`, `*`, comparison and
/// integer arithmetic — so it is bit-identical on every IEEE-754 target without
/// needing rule 4's quantisation.
#[must_use]
pub fn simplex2_with_gradient(seed: u64, x: f64, y: f64) -> (f64, f64, f64) {
    if !x.is_finite() || !y.is_finite() {
        return (0.0, 0.0, 0.0);
    }

    // Skew the input into the lattice and find the cell.
    let skew = (x + y) * F2;
    let cell_i = (x + skew).floor();
    let cell_j = (y + skew).floor();
    let unskew = (cell_i + cell_j) * G2;

    // Offsets from the first corner, in unskewed space.
    let d0x = x - (cell_i - unskew);
    let d0y = y - (cell_j - unskew);

    // Which of the two triangles in the cell the point is in.
    let (step_i, step_j) = if d0x > d0y {
        (1_i64, 0_i64)
    } else {
        (0_i64, 1_i64)
    };
    #[allow(clippy::cast_possible_truncation)] // floor of a finite f64; see below
    let (base_i, base_j) = (cell_i as i64, cell_j as i64);

    #[allow(clippy::cast_precision_loss)] // 0 or 1
    let (step_ix, step_jy) = (step_i as f64, step_j as f64);
    let d1x = d0x - step_ix + G2;
    let d1y = d0y - step_jy + G2;
    let d2x = d0x - 1.0 + 2.0 * G2;
    let d2y = d0y - 1.0 + 2.0 * G2;

    // Fixed summation order: corner 0, then 1, then 2. Re-associating this sum
    // changes the last bit of every height in the city (rule 3).
    let (n0, gx0, gy0) = corner_contribution(seed, base_i, base_j, d0x, d0y);
    let (n1, gx1, gy1) = corner_contribution(seed, base_i + step_i, base_j + step_j, d1x, d1y);
    let (n2, gx2, gy2) = corner_contribution(seed, base_i + 1, base_j + 1, d2x, d2y);

    let value = NOISE_SCALE * (n0 + n1 + n2);
    let ddx = NOISE_SCALE * (gx0 + gx1 + gx2);
    let ddy = NOISE_SCALE * (gy0 + gy1 + gy2);

    // The scale is calibrated to keep this inside the range; the clamp makes the
    // documented `[-1, 1]` a guarantee rather than a measurement.
    (value.clamp(-1.0, 1.0), ddx, ddy)
}

/// 2-D simplex noise in `[-1, 1]`, in `f64`.
#[must_use]
pub fn simplex2_f64(seed: u64, x: f64, y: f64) -> f64 {
    simplex2_with_gradient(seed, x, y).0
}

/// 2-D simplex noise in `[-1, 1]`, written out (ADR-0050).
///
/// Classic 2-D simplex: skew into the simplex lattice, take the three corners,
/// look up a gradient per corner from a hash of `(seed, corner)`, and sum the
/// radially-attenuated contributions.
///
/// The `f32` form for callers that already hold a [`Point`]. The arithmetic is
/// `f64` throughout and is rounded once on the way out ([`narrow`]);
/// [`simplex2_f64`] is the form to use when sampling in `f64`, which
/// [`crate::terrain`] does.
#[must_use]
pub fn simplex2(seed: u64, at: Point) -> f32 {
    narrow(simplex2_f64(seed, f64::from(at.x), f64::from(at.y)))
}

/// Fractal Brownian motion over [`simplex2`] with its analytic gradient, in
/// `f64`. Returns `(value, d/dx, d/dy)`, with the value in `[-1, 1]`.
///
/// Each octave doubles the frequency, halves the amplitude, and takes a seed
/// derived from the base seed — so two octaves cannot line up their lattices and
/// produce a visible grid. The sum is normalised by the total amplitude, which
/// keeps the range independent of the octave count: changing `octaves` changes
/// the texture, never the scale.
#[must_use]
pub fn fbm2_with_gradient(seed: u64, x: f64, y: f64, octaves: u32) -> (f64, f64, f64) {
    let octaves = octaves.min(MAX_OCTAVES);
    if octaves == 0 {
        return (0.0, 0.0, 0.0);
    }

    let mut value = 0.0;
    let mut ddx = 0.0;
    let mut ddy = 0.0;
    let mut amplitude = 1.0_f64;
    let mut frequency = 1.0_f64;
    let mut total = 0.0_f64;

    // Ascending octave order, fixed. Every term is a power of two, so the
    // frequency and amplitude sequences are exact and the only rounding in the
    // loop is the sample itself.
    for octave in 0..octaves {
        let octave_seed = combine_seeds(seed, u64::from(octave));
        let (sample, sx, sy) = simplex2_with_gradient(octave_seed, x * frequency, y * frequency);
        value += amplitude * sample;
        ddx += amplitude * frequency * sx;
        ddy += amplitude * frequency * sy;
        total += amplitude;
        amplitude *= 0.5;
        frequency *= 2.0;
    }

    let inv = 1.0 / total;
    (value * inv, ddx * inv, ddy * inv)
}

/// Fractal Brownian motion over [`simplex2`] — several octaves at halving
/// amplitude and doubling frequency.
///
/// PRD §7.2 asks for **low-frequency** terrain. Two or three octaves is the
/// whole budget: more octaves make the field wrinkly, roads follow the wrinkles,
/// and the result reads as noise sprinkled on a grid, which is exactly what
/// PRD §7 says not to build. [`TERRAIN_OCTAVES`] is the value the city uses;
/// anything above [`MAX_OCTAVES`] is clamped.
#[must_use]
pub fn fbm2(seed: u64, at: Point, octaves: u32) -> f32 {
    narrow(fbm2_with_gradient(seed, f64::from(at.x), f64::from(at.y), octaves).0)
}

/// [`fbm2`] in `f64`.
#[must_use]
pub fn fbm2_f64(seed: u64, x: f64, y: f64, octaves: u32) -> f64 {
    fbm2_with_gradient(seed, x, y, octaves).0
}

/// The analytic gradient of [`fbm2`] at a point.
///
/// Road segments follow this where the slope exceeds a threshold (PRD §7.2), so
/// it is on the hot path of road growth. It is **analytic**, not a finite
/// difference: the corner weights the value already computes are exactly what
/// the derivative needs, so it costs almost nothing, and it avoids baking a
/// step size into the layout as a second tuning parameter.
///
/// This is the mathematical gradient and therefore points **uphill**.
/// [`crate::terrain::TerrainField::gradient`] negates it, because roads follow
/// the descent direction.
#[must_use]
pub fn fbm2_gradient(seed: u64, at: Point, octaves: u32) -> Vec2 {
    let (_, ddx, ddy) = fbm2_with_gradient(seed, f64::from(at.x), f64::from(at.y), octaves);
    Vec2::new(narrow(ddx), narrow(ddy))
}

#[cfg(test)]
mod tests {
    // Every float comparison in this module is an exact, bit-level assertion —
    // that is what a determinism suite is for (PRD §7.4, §16). `float_cmp`
    // exists to catch approximate equality written as `==`, which is the
    // opposite of what is happening here; anything that genuinely needs a
    // tolerance in these tests is written with one explicitly.
    #![allow(clippy::float_cmp)]

    use super::*;

    fn path(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("valid logical path")
    }

    // -----------------------------------------------------------------------
    // The hash — pinned, and proven not to be DefaultHasher
    // -----------------------------------------------------------------------

    /// An independent second implementation of FNV-1a, written from the
    /// specification rather than from [`fnv1a64`]. A refactor of the real one
    /// that changes its behaviour has to change this too, and changing both to
    /// agree on a new answer is a thing nobody does by accident.
    fn reference_fnv1a64(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 14_695_981_039_346_656_037; // 0xcbf29ce484222325
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(1_099_511_628_211); // 0x100000001b3
        }
        hash
    }

    #[test]
    fn hash_is_pinned_to_literal_values() {
        // If these fail, every golden layout file in the repo is invalid — on
        // purpose (ADR-0029). The first three are the published FNV-1a test
        // vectors, so a failure also says whether the algorithm drifted or only
        // this crate's use of it did.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
        assert_eq!(fnv1a64_str("src/auth.ts"), 0xfd70_4eea_30dc_76c3);
        assert_eq!(fnv1a64_str("rotation"), 0xb51a_fb05_cd34_709f);
    }

    #[test]
    fn hash_agrees_with_an_independent_implementation() {
        for sample in [
            &b""[..],
            b"a",
            b"foobar",
            b"src/auth.ts",
            b"polis-layout/src/determinism.rs",
            &[0u8, 255, 128, 1],
        ] {
            assert_eq!(fnv1a64(sample), reference_fnv1a64(sample));
        }
    }

    /// The regression this file exists to prevent.
    ///
    /// Someone reaches for `DefaultHasher` because it is in `std` and "a hash is
    /// a hash". `DefaultHasher` is SipHash-1-3, whose output `std` explicitly
    /// refuses to guarantee across releases — swapping it in would keep every
    /// test that only checks *self-consistency* green while silently making the
    /// city depend on the toolchain. This test fails the moment [`fnv1a64`]
    /// starts agreeing with it, and the pinned literals above fail the moment it
    /// stops agreeing with FNV-1a.
    ///
    /// `DefaultHasher` is constructed here and nowhere else in the crate.
    #[test]
    fn hash_is_not_default_hasher() {
        use std::hash::Hasher as _;

        for sample in [&b"src/auth.ts"[..], b"rotation", b"roof"] {
            let mut sip = std::collections::hash_map::DefaultHasher::new();
            sip.write(sample);
            assert_ne!(
                fnv1a64(sample),
                sip.finish(),
                "determinism::fnv1a64 is behaving like DefaultHasher; \
                 SipHash's output is not stable across Rust releases (ADR-0029)"
            );
        }
    }

    #[test]
    fn mixer_is_injective_on_a_sample() {
        // A collision would silently merge two draw sites into one.
        let mut seen = std::collections::BTreeSet::new();
        for i in 0..20_000u64 {
            assert!(seen.insert(mix64(i)), "mix64 collided at {i}");
        }
        // Pinned. `mix64(0) == 0` is a property of the SplitMix64 finalizer, not
        // a bug: `SeededRng` adds `GOLDEN_GAMMA` before mixing, so a zero seed
        // still produces a full stream.
        assert_eq!(mix64(0), 0);
        assert_eq!(mix64(1), 0x5692_161d_100b_05e5);
    }

    #[test]
    fn seed_depends_on_path_and_purpose() {
        let rotation = seed_for_path(&path("src/auth.ts"), "rotation");
        let roof = seed_for_path(&path("src/auth.ts"), "roof");
        let other = seed_for_path(&path("src/other.ts"), "rotation");
        assert_ne!(rotation, roof, "purpose must separate draw sites");
        assert_ne!(rotation, other, "path must separate draw sites");
        // Case folding comes from `layout_seed`; two spellings are one file.
        assert_eq!(rotation, seed_for_path(&path("SRC/Auth.TS"), "rotation"));
        // Pinned (ADR-0029).
        assert_eq!(rotation, 0xf8b7_bbe7_4626_63ba);
    }

    #[test]
    fn combine_seeds_is_order_dependent() {
        assert_ne!(combine_seeds(1, 2), combine_seeds(2, 1));
        assert_ne!(combine_seeds(0, 0), 0);
    }

    // -----------------------------------------------------------------------
    // The generator
    // -----------------------------------------------------------------------

    #[test]
    fn rng_stream_is_pinned() {
        let mut rng = SeededRng::for_path(&path("src/auth.ts"), "rotation");
        let drawn: Vec<u64> = (0..4).map(|_| rng.next_u64()).collect();
        assert_eq!(
            drawn,
            vec![
                0xe148_7f81_a3f9_7466,
                0xf41f_cbca_547a_adc4,
                0xc2d2_3616_3664_f55e,
                0xd97e_62f1_3f8e_8806,
            ]
        );
    }

    #[test]
    fn rng_is_reproducible_and_stream_separated() {
        let file = path("src/auth.ts");
        let mut first = SeededRng::for_path(&file, "rotation");
        let mut second = SeededRng::for_path(&file, "rotation");
        for _ in 0..64 {
            assert_eq!(first.next_u64(), second.next_u64());
        }

        let mut roof = SeededRng::for_path(&file, "roof");
        let mut rotation = SeededRng::for_path(&file, "rotation");
        let roofs: Vec<u64> = (0..16).map(|_| roof.next_u64()).collect();
        let rotations: Vec<u64> = (0..16).map(|_| rotation.next_u64()).collect();
        assert_ne!(roofs, rotations);
    }

    #[test]
    fn unit_interval_draws_stay_in_range() {
        let mut rng = SeededRng::for_seed(7, "range");
        for _ in 0..200_000 {
            let f32_draw = rng.next_f32();
            assert!(
                (0.0..1.0).contains(&f32_draw),
                "f32 draw out of range: {f32_draw}"
            );
            let f64_draw = rng.next_f64();
            assert!(
                (0.0..1.0).contains(&f64_draw),
                "f64 draw out of range: {f64_draw}"
            );
        }
    }

    #[test]
    fn range_is_half_open_and_total() {
        let mut rng = SeededRng::for_seed(11, "range");
        for _ in 0..100_000 {
            let value = rng.range_f32(-4.0, 4.0);
            assert!((-4.0..4.0).contains(&value), "{value}");
        }
        // Degenerate ranges return `low` rather than a NaN that deletes a
        // building.
        assert_eq!(rng.range_f32(3.0, 3.0), 3.0);
        assert_eq!(rng.range_f32(3.0, 1.0), 3.0);
        assert_eq!(rng.range_f32(2.0, f32::NAN), 2.0);
        assert_eq!(rng.range_f32(2.0, f32::INFINITY), 2.0);
        assert!(rng.range_f32(f32::NAN, 1.0).is_nan());
    }

    #[test]
    fn below_is_uniform_and_total() {
        assert_eq!(SeededRng::for_seed(1, "n").below(0), 0);
        assert_eq!(SeededRng::for_seed(1, "n").below(1), 0);

        // Roof forms: PRD §7.3 varies them by a hash of the path, and a subtly
        // non-uniform selection is worse than an obviously wrong one.
        let mut counts = [0u32; 3];
        let mut rng = SeededRng::for_seed(3, "roof");
        for _ in 0..300_000 {
            counts[usize::try_from(rng.below(3)).expect("below(3) fits")] += 1;
        }
        for count in counts {
            assert!(
                (99_000..101_000).contains(&count),
                "biased selection: {counts:?}"
            );
        }
    }

    #[test]
    fn choose_is_deterministic_and_handles_empty() {
        let items = ["flat", "stepped", "pitched"];
        let mut rng = SeededRng::for_path(&path("src/auth.ts"), "roof");
        let picked = *rng.choose(&items).expect("non-empty");
        let mut again = SeededRng::for_path(&path("src/auth.ts"), "roof");
        assert_eq!(picked, *again.choose(&items).expect("non-empty"));
        assert_eq!(rng.choose::<u8>(&[]), None);
    }

    #[test]
    fn sub_streams_are_independent_and_do_not_advance_the_parent() {
        let parent = SeededRng::for_seed(42, "block");
        let mut left = parent.sub("lots");
        let mut right = parent.sub("setback");
        assert_ne!(left.next_u64(), right.next_u64());

        let mut before = parent.clone();
        let _ = parent.sub("anything");
        let mut after = parent;
        assert_eq!(before.next_u64(), after.next_u64());
    }

    #[test]
    fn indexed_streams_do_not_shift_when_a_neighbour_is_inserted() {
        // The property the whole "seed per draw site" rule buys: element 7 keeps
        // its numbers when element 3 appears.
        let file = path("src/auth.ts");
        let seventh = SeededRng::for_path_indexed(&file, "lot", 7).next_u64();
        let third = SeededRng::for_path_indexed(&file, "lot", 3).next_u64();
        assert_ne!(seventh, third);
        assert_eq!(
            seventh,
            SeededRng::for_path_indexed(&file, "lot", 7).next_u64()
        );
    }

    #[test]
    fn unit_vectors_are_unit_length() {
        let mut rng = SeededRng::for_seed(5, "direction");
        for _ in 0..5_000 {
            let v = rng.unit_vector();
            assert!((v.length() - 1.0).abs() < 1e-5, "{v:?}");
        }
    }

    /// The only place in this crate where a transcendental function's output can
    /// reach the layout, pinned in full.
    ///
    /// [`det_sin_cos`]'s quantisation makes two libms *very likely* to agree; it
    /// cannot prove they do. [`SeededRng::unit_vector`] has only
    /// [`UNIT_DIRECTIONS`] possible inputs, so the whole table can be hashed and
    /// pinned — which converts "very likely" into "a machine that disagrees
    /// fails this test loudly instead of quietly building a different city".
    ///
    /// Apply the same treatment to any future draw site that needs an angle: fix
    /// the set of inputs, then pin the outputs.
    #[test]
    fn the_direction_table_is_pinned() {
        let mut digest = fnv1a64(b"polis unit direction table v1");
        for index in 0..UNIT_DIRECTIONS {
            let turns = f64::from(index) / f64::from(UNIT_DIRECTIONS);
            let (sin, cos) = det_sin_cos(turns * TAU);
            digest = combine_seeds(digest, sin.to_bits());
            digest = combine_seeds(digest, cos.to_bits());
        }
        assert_eq!(
            digest, 0xbb6b_7e26_8fec_e9f1,
            "a platform's sin/cos disagrees past TRIG_QUANTUM, or the table changed"
        );
    }

    #[test]
    fn trig_is_pinned_at_representative_angles() {
        // `det_sin_cos` is the crate's only sanctioned route to a sine, so its
        // grid is pinned too: quadrant boundaries, where a libm's argument
        // reduction is at its least agreeable, plus an ordinary angle.
        for (radians, sin, cos) in [
            (0.0_f64, 0.0_f64, 1.0_f64),
            (std::f64::consts::FRAC_PI_2, 1.0, 0.0),
            (std::f64::consts::PI, 0.0, -1.0),
            (1.0, 0.841_471, 0.540_302_3),
            (-2.5, -0.598_472_1, -0.801_143_6),
        ] {
            let (got_sin, got_cos) = det_sin_cos(radians);
            assert_eq!(got_sin.to_bits(), sin.to_bits(), "sin({radians})");
            assert_eq!(got_cos.to_bits(), cos.to_bits(), "cos({radians})");
        }
    }

    // -----------------------------------------------------------------------
    // Float discipline
    // -----------------------------------------------------------------------

    #[test]
    fn quantize_normalises_negative_zero() {
        // The most annoying possible golden-file failure: two values that
        // compare equal and serialize differently.
        assert_eq!(quantize(-0.0).to_bits(), 0.0f32.to_bits());
        assert_eq!(quantize(-0.0001).to_bits(), 0.0f32.to_bits());
        assert_eq!(quantize_f64(-0.0).to_bits(), 0.0f64.to_bits());
    }

    #[test]
    fn quantize_is_total_over_non_finite_input() {
        assert_eq!(quantize(f32::NAN), 0.0);
        assert_eq!(quantize(f32::INFINITY), 0.0);
        assert_eq!(quantize(f32::NEG_INFINITY), 0.0);
    }

    #[test]
    fn quantize_collapses_last_bit_differences() {
        // The failure PRD §16's two-OS comparison would otherwise hit: two
        // accumulations that differ in the last bit must serialize identically.
        let value = 123.456_f32;
        let nudged = f32::from_bits(value.to_bits() + 1);
        assert_ne!(value.to_bits(), nudged.to_bits());
        assert_eq!(quantize(value).to_bits(), quantize(nudged).to_bits());
        assert!((quantize(value) - value).abs() <= QUANTUM);
    }

    #[test]
    fn quantize_point_and_vec_round_both_components() {
        let p = quantize_point(Point::new(1.000_4, -0.000_2));
        assert_eq!(p.x, 1.0);
        assert_eq!(p.y.to_bits(), 0.0f32.to_bits());
        let v = quantize_vec2(Vec2::new(-2.000_6, 0.0));
        assert_eq!(v.x, -2.001);
        assert_eq!(v.y.to_bits(), 0.0f32.to_bits());
    }

    #[test]
    fn quantum_and_its_scale_agree() {
        // `QUANTUM` is the documented step and `QUANTUM_SCALE` is its exact
        // reciprocal; they differ only by `0.001`'s f32 representation error.
        assert!((f64::from(QUANTUM) - 1.0 / QUANTUM_SCALE).abs() < 1e-9);
        // And the grid `quantize` actually snaps to is `QUANTUM` wide.
        assert_eq!(quantize(0.0015), 0.002);
        assert_eq!(quantize(0.0014), 0.001);
    }

    #[test]
    fn trig_helpers_are_quantised_and_consistent() {
        let (sin, cos) = det_sin_cos(1.0);
        assert!((sin - 1.0_f64.sin()).abs() <= TRIG_QUANTUM);
        assert!((cos - 1.0_f64.cos()).abs() <= TRIG_QUANTUM);
        // On the grid: multiplying by 1/TRIG_QUANTUM gives an integer.
        assert!(((sin / TRIG_QUANTUM).round() - sin / TRIG_QUANTUM).abs() < 1e-6);

        let v = det_from_angle(0.7);
        assert!((v.length() - 1.0).abs() < 1e-5);
        assert!((det_angle(v) - 0.7).abs() < 1e-5);
        assert_eq!(det_angle(Vec2::ZERO), 0.0);
    }

    #[test]
    fn narrow_is_the_only_rounding() {
        assert_eq!(narrow(1.0), 1.0_f32);
        assert!(narrow(f64::NAN).is_nan());
        assert_eq!(narrow(f64::INFINITY), f32::INFINITY);
    }

    #[test]
    fn debug_assert_finite_accepts_finite_values() {
        debug_assert_finite(1.0, "test");
        debug_assert_finite(0.0, "test");
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "non-finite value reached the layout")]
    fn debug_assert_finite_catches_nan() {
        debug_assert_finite(f32::NAN, "test");
    }

    // -----------------------------------------------------------------------
    // Ordering
    // -----------------------------------------------------------------------

    #[test]
    fn sorting_by_a_float_key_is_stable_and_nan_safe() {
        let mut items = vec![(3.0_f32, 'a'), (f32::NAN, 'b'), (1.0, 'c'), (1.0, 'd')];
        sort_by_f32_key(&mut items, |item| item.0);
        // NaN sorts last under `total_cmp`; equal keys keep input order.
        assert_eq!(items[0].1, 'c');
        assert_eq!(items[1].1, 'd');
        assert_eq!(items[2].1, 'a');
        assert!(items[3].0.is_nan());

        let mut floats = vec![2.0_f64, -1.0, 0.5];
        sort_by_f64_key(&mut floats, |v| *v);
        assert_eq!(floats, vec![-1.0, 0.5, 2.0]);
    }

    #[test]
    fn canonical_order_sorts_and_dedups() {
        let laundered = canonical_order(vec!["b", "a", "b", "c"]);
        assert_eq!(laundered, vec!["a", "b", "c"]);
        debug_assert_canonical_order(&laundered, "test");
    }

    /// The debug assertion that catches a `HashMap` reaching layout output.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "is not in canonical order")]
    fn debug_assert_canonical_order_catches_hash_iteration() {
        // Not a synthetic unsorted vector: a real `HashMap` iteration, which is
        // exactly the mistake the assertion exists to catch. `RandomState` makes
        // the order differ per process, so with this many keys the odds of an
        // accidentally sorted iteration are negligible.
        let set: std::collections::HashSet<u32> = (0..64).collect();
        let keys: Vec<u32> = set.iter().copied().collect();
        debug_assert_canonical_order(&keys, "district keys");
    }

    // -----------------------------------------------------------------------
    // Noise
    // -----------------------------------------------------------------------

    #[test]
    fn simplex_is_pinned_to_literal_values() {
        // ADR-0050. A failure here means the terrain field moved, which means
        // every road, block, lot and building moved. Compared bit for bit, not
        // within a tolerance: PRD §16 compares golden files byte for byte, so a
        // last-bit change here is exactly as significant as a large one.
        for (x, y, expected) in SIMPLEX_PINS {
            let got = simplex2_f64(PIN_SEED, x, y);
            assert_eq!(
                got.to_bits(),
                expected.to_bits(),
                "simplex2_f64({PIN_SEED:#x}, {x}, {y}) = {got:?}, pinned at {expected:?}"
            );
        }
    }

    #[test]
    fn simplex_is_zero_at_a_lattice_point() {
        // A gradient-noise property worth stating: at a lattice point the only
        // corner in range is the one at zero offset, and its dot product is
        // zero. If this ever returns something non-zero, the corner geometry
        // changed even if the pins somehow still pass.
        assert_eq!(simplex2_f64(PIN_SEED, 0.0, 0.0), 0.0);
        assert_eq!(fbm2_f64(PIN_SEED, 0.0, 0.0, TERRAIN_OCTAVES), 0.0);
    }

    #[test]
    fn fbm_is_pinned_to_literal_values() {
        for (x, y, expected) in FBM_PINS {
            let got = fbm2_f64(PIN_SEED, x, y, TERRAIN_OCTAVES);
            assert_eq!(
                got.to_bits(),
                expected.to_bits(),
                "fbm2_f64({PIN_SEED:#x}, {x}, {y}, {TERRAIN_OCTAVES}) = {got:?}, \
                 pinned at {expected:?}"
            );
        }
    }

    #[test]
    fn noise_stays_in_range_and_is_finite() {
        let mut extreme: f64 = 0.0;
        let mut rng = SeededRng::for_seed(1, "noise sample");
        for _ in 0..200_000 {
            let x = rng.range_f64(-500.0, 500.0);
            let y = rng.range_f64(-500.0, 500.0);
            let (value, ddx, ddy) = simplex2_with_gradient(PIN_SEED, x, y);
            assert!(value.is_finite() && ddx.is_finite() && ddy.is_finite());
            assert!((-1.0..=1.0).contains(&value), "{value} at ({x}, {y})");
            extreme = extreme.max(value.abs());

            let fbm = fbm2_f64(PIN_SEED, x, y, TERRAIN_OCTAVES);
            assert!(fbm.is_finite() && (-1.0..=1.0).contains(&fbm));
        }
        // The clamp must be a formality, not load-bearing: if the raw sum
        // overshot, the field would be flat-topped and the clamp would hide it.
        assert!(extreme > 0.9, "noise is wasting its range: peak {extreme}");
    }

    #[test]
    fn noise_is_total_over_non_finite_input() {
        assert_eq!(simplex2_f64(PIN_SEED, f64::NAN, 0.0), 0.0);
        assert_eq!(simplex2_f64(PIN_SEED, f64::INFINITY, 0.0), 0.0);
        assert_eq!(fbm2_f64(PIN_SEED, 0.0, f64::NAN, 3), 0.0);
    }

    #[test]
    fn noise_is_low_frequency_and_not_a_lattice() {
        // Two nearby samples must be close — a field that jumps between
        // neighbouring points gives roads nothing to follow.
        let mut worst: f64 = 0.0;
        let mut at = -20.0_f64;
        while at < 20.0 {
            let here = simplex2_f64(PIN_SEED, at, 3.25);
            let there = simplex2_f64(PIN_SEED, at + 0.01, 3.25);
            worst = worst.max((here - there).abs());
            at += 0.01;
        }
        assert!(worst < 0.1, "field is not smooth: {worst} over a 0.01 step");

        // And it must not be constant along the lattice axes.
        let axis: Vec<f64> = (0..16)
            .map(|i| simplex2_f64(PIN_SEED, f64::from(i), 0.0))
            .collect();
        assert!(
            axis.iter().any(|v| v.abs() > 0.05),
            "field is flat on the x axis"
        );
    }

    #[test]
    fn octave_count_does_not_change_the_scale() {
        // Normalising by the total amplitude is what makes `octaves` a texture
        // knob rather than a scale knob.
        for octaves in 1..=MAX_OCTAVES {
            let mut peak: f64 = 0.0;
            for i in 0..4_000 {
                let t = f64::from(i) * 0.01;
                peak = peak.max(fbm2_f64(PIN_SEED, t, t * 0.7, octaves).abs());
            }
            assert!(peak <= 1.0, "octaves={octaves} exceeded the range: {peak}");
            assert!(peak > 0.3, "octaves={octaves} collapsed the range: {peak}");
        }
        assert_eq!(fbm2_f64(PIN_SEED, 1.0, 1.0, 0), 0.0);
        // Above MAX_OCTAVES the count is clamped, not honoured.
        assert_eq!(
            fbm2_f64(PIN_SEED, 1.0, 1.0, MAX_OCTAVES),
            fbm2_f64(PIN_SEED, 1.0, 1.0, MAX_OCTAVES + 40)
        );
    }

    /// The analytic gradient must agree with the field it claims to differentiate.
    ///
    /// Property-style: many pseudo-random points drawn from the crate's own
    /// generator (so the sample set is itself deterministic and a failure is
    /// reproducible from the seed alone), each checked against a central finite
    /// difference. `proptest` is not a dependency of this crate, and a
    /// determinism suite that used a randomly-seeded property runner would be
    /// arguing against itself.
    #[test]
    fn gradient_matches_the_field_it_differentiates() {
        const STEP: f64 = 1e-5;
        let mut rng = SeededRng::for_seed(99, "gradient property");
        let mut worst: f64 = 0.0;
        for _ in 0..20_000 {
            let x = rng.range_f64(-40.0, 40.0);
            let y = rng.range_f64(-40.0, 40.0);
            for octaves in [1, TERRAIN_OCTAVES] {
                let (_, ddx, ddy) = fbm2_with_gradient(PIN_SEED, x, y, octaves);
                let numeric_x = (fbm2_f64(PIN_SEED, x + STEP, y, octaves)
                    - fbm2_f64(PIN_SEED, x - STEP, y, octaves))
                    / (2.0 * STEP);
                let numeric_y = (fbm2_f64(PIN_SEED, x, y + STEP, octaves)
                    - fbm2_f64(PIN_SEED, x, y - STEP, octaves))
                    / (2.0 * STEP);
                worst = worst
                    .max((ddx - numeric_x).abs())
                    .max((ddy - numeric_y).abs());
            }
        }
        assert!(
            worst < 1e-4,
            "analytic gradient disagrees with the field by {worst}"
        );
    }

    #[test]
    fn gradient_points_uphill() {
        // The sign convention `terrain::TerrainField::gradient` inverts.
        let mut rng = SeededRng::for_seed(123, "uphill");
        for _ in 0..2_000 {
            let x = rng.range_f64(-30.0, 30.0);
            let y = rng.range_f64(-30.0, 30.0);
            let (value, ddx, ddy) = fbm2_with_gradient(PIN_SEED, x, y, TERRAIN_OCTAVES);
            let length = (ddx * ddx + ddy * ddy).sqrt();
            if length < 1e-3 {
                continue;
            }
            let step = 1e-4;
            let uphill = fbm2_f64(
                PIN_SEED,
                x + ddx / length * step,
                y + ddy / length * step,
                TERRAIN_OCTAVES,
            );
            assert!(uphill >= value, "gradient pointed downhill at ({x}, {y})");
        }
    }

    #[test]
    fn f32_wrappers_agree_with_the_f64_forms() {
        let at = Point::new(1.25, -3.5);
        assert_eq!(simplex2(7, at), narrow(simplex2_f64(7, 1.25, -3.5)));
        assert_eq!(fbm2(7, at, 3), narrow(fbm2_f64(7, 1.25, -3.5, 3)));
        let gradient = fbm2_gradient(7, at, 3);
        let (_, ddx, ddy) = fbm2_with_gradient(7, 1.25, -3.5, 3);
        assert_eq!(gradient.x, narrow(ddx));
        assert_eq!(gradient.y, narrow(ddy));
    }

    // -----------------------------------------------------------------------
    // Pins
    // -----------------------------------------------------------------------

    /// The seed the pinned noise values were produced with. Arbitrary and
    /// fixed — a pin should fail when the noise changes, not when a fixture is
    /// renamed.
    const PIN_SEED: u64 = 0x0bad_c0de_dead_beef;

    /// `(x, y, simplex2_f64(PIN_SEED, x, y))`, written out (ADR-0050, rule 7).
    ///
    /// The points are chosen to exercise the parts of the algorithm that a
    /// plausible refactor would break: both halves of the cell (the `d0x > d0y`
    /// split), a negative quadrant, a coordinate far from the origin where the
    /// skew has accumulated, and a point close enough to a lattice point that
    /// only one corner is in range.
    const SIMPLEX_PINS: [(f64, f64, f64); 5] = [
        (0.5, 0.5, -0.434_407_069_272_777_17),
        (1.0, 0.0, 0.595_134_665_227_665_7),
        (-3.25, 7.125, -0.124_328_949_042_561_5),
        (128.5, -64.25, -0.746_665_657_642_877_6),
        (0.000_001, 0.000_002, -4.375_223_208_416_754e-6),
    ];

    /// `(x, y, fbm2_f64(PIN_SEED, x, y, TERRAIN_OCTAVES))`, written out.
    const FBM_PINS: [(f64, f64, f64); 3] = [
        (1.5, -2.5, -0.675_774_338_702_866_6),
        (12.25, 33.75, 0.222_736_334_449_783_16),
        (-0.75, 0.25, -0.228_442_673_429_827_88),
    ];
}
