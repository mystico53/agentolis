//! Determinism (PRD §7.4) — the hard requirement everything else rests on.
//!
//! > **Every random draw is seeded from a hash of the logical path.** Never from
//! > wall clock, never from a global RNG, never from iteration order of a
//! > `HashMap`.
//!
//! > Use `BTreeMap` wherever iteration order can affect layout. This is a common
//! > source of nondeterminism and it will be subtle when it bites.
//!
//! The generator below is written out rather than pulled from `rand`, for the
//! same reason [`polis_events::LogicalPath::layout_seed`] does not use
//! `DefaultHasher`: a dependency may change its algorithm in a patch release and
//! silently reshuffle every city in existence. PRD §7.4 promises "the same repo
//! produces the same city on every launch and on every machine", which includes
//! machines on a different toolchain and a different lockfile.

use polis_events::LogicalPath;

/// A seeded, path-local pseudo-random generator.
///
/// One is constructed per draw site from `(path, purpose)`, so adding a new draw
/// somewhere in the pipeline cannot shift the numbers every later draw sees —
/// which a single shared stream would.
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
    pub fn for_path(path: &LogicalPath, purpose: &str) -> Self {
        let _ = (path, purpose);
        todo!("PRD §7.4 — mix layout_seed with a hash of the purpose tag")
    }

    /// Next raw value.
    pub fn next_u64(&mut self) -> u64 {
        let _ = self.state;
        todo!("PRD §7.4 — a fixed, written-out generator, never a crate default")
    }

    /// Uniform in `[0, 1)`.
    pub fn next_f32(&mut self) -> f32 {
        todo!("PRD §7.4")
    }

    /// Uniform in `[low, high)`.
    pub fn range_f32(&mut self, low: f32, high: f32) -> f32 {
        let _ = (low, high);
        todo!("PRD §7.4")
    }
}

/// Rounds a coordinate to the layout grid before it is serialised.
///
/// Floating-point accumulation differs between targets. PRD §16 compares golden
/// layout files across two operating systems byte for byte, so every coordinate
/// that reaches the snapshot is quantised first — otherwise the determinism test
/// fails on a last-bit difference no human could act on.
pub fn quantize(value: f32) -> f32 {
    let _ = value;
    todo!("PRD §16 — fixed-point quantisation before serialisation")
}
