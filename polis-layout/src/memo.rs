//! What a growth step remembers, so it does not redo work nothing changed.
//!
//! > Growth is genuinely incremental — a new file runs one growth step, it does
//! > not regenerate the world. (PRD §7.4)
//!
//! `City::accrete` re-runs `city::assemble` after one file lands, and PRD §13.1
//! budgets that at **under 50 ms, off-thread**. Measured at 5 000 files before
//! this module existed: a single add moved a *median of four* of 1 165 road
//! nodes and cost 44 ms, of which 11 ms re-cut 866 blocks that were byte-for-
//! byte identical to the blocks of the previous step and 30 ms of CPU re-seated
//! 5 474 buildings on parcels that had not moved. The step was recomputing
//! essentially the whole city to record a change to a thousandth of it.
//!
//! # The boundary matters as much as the cache
//!
//! [`CutCache`] first remembered `crate::lots::subdivide_weighted`'s bare rings.
//! That is **0.4 ms** of a growth step. The 7.1 ms that turns those rings into
//! parcels — probing each for an interior point clear of the carriageway,
//! splitting the roomiest again until the block has one per file, and the
//! frontage sort — was outside it, and so was paid on every step whether
//! anything had moved or not. The stored value is a [`BlockCut`] now: the same
//! key with one extra word in it, and six milliseconds a step that were being
//! spent re-deriving parcels nobody had touched.
//!
//! # These are caches of pure functions, not an approximation
//!
//! Both stages memoised here are pure functions of their arguments and read
//! nothing any other block or parcel writes — that is the same property
//! `city::assemble` already relies on to run the building stage in parallel.
//! Two calls with byte-identical arguments therefore have byte-identical
//! results, and remembering one is not an estimate of anything.
//!
//! **Every hit is verified exactly.** The 64-bit digest picks a bucket; the
//! stored arguments are then compared *bit for bit* (`f64::to_bits`, never `==`,
//! so `-0.0` cannot match `0.0` and a `NaN` cannot match itself) before the
//! stored result is returned. A digest collision costs a recomputation and can
//! never produce a wrong city. Nothing here can weaken PRD §7.4: a cache that
//! cannot change an output can only skip producing one already known.
//!
//! # Why a cache and not a smaller recomputation
//!
//! Because *which* blocks change is not known until they have been rebuilt. The
//! road graph is welded from the whole cell set and its faces are re-walked, so
//! an add cannot be localised upstream. Comparing the rebuilt arguments against
//! the previous step's is the cheap, exact way to discover what genuinely moved.
//!
//! # A cache cannot help with work that genuinely has to be redone
//!
//! This module's header used to end there, and the two hit rates it quoted —
//! 99 % on a step that leaves the block count alone, 55 % on a step that changes
//! it — hid the fact that most of that 45 % was **not** a real change. Two seeds
//! in the pipeline were derived from an array index, so inserting anything
//! re-rolled the dice for everything after it:
//!
//! * `crate::lots::block_cut_seed` was `first file's path hash ^ block index * 31`;
//! * `crate::roads::edge_identity` (the prune coin) was seeded from an edge's two
//!   **node** indices, which the weld renumbers.
//!
//! Both are geometric or path-derived now, and both are documented where they
//! live. Measured at 5 000 files, one added file: **242 of ~900 blocks changed
//! shape before, 49 after**, and the buildings that had to be re-seated on a
//! typical step fell from a quarter of the city to 3-13 of 4 577.
//!
//! What is left is real. On the six adds in twelve that settle a *new plot*,
//! `crate::regions::partition` reassigns plots between districts and
//! `regions::seat_files` then moves files between plots, so ~200 blocks of ~900
//! get a different file list and legitimately have to be cut and seated again.
//! No cache can help with that: the arguments changed. Making the partition
//! incremental is the next fix, and it is upstream of this module.
//!
//! Measured hit rates now, at 5 000 files over twelve adds: **99.9 %** on a step
//! that seats a file into an existing plot, 45-95 % on one that settles a new
//! plot, 77 % (cuts) and 90 % (buildings) averaged.
//!
//! # Bounded
//!
//! [`SeatCache`] is **rebuilt from scratch on every assembly**: it holds exactly
//! the answers of the step that produced it and nothing older, so it is bounded
//! by the city and needs no eviction policy at all. [`CutCache`] accumulates
//! instead — a block that comes back after a step or two is worth keeping — and
//! is emptied wholesale once it holds more than [`SLACK`] times the block count.
//! A size rule rather than an eviction policy: it costs one slow growth step,
//! and it is the same rule on every machine.

use std::collections::BTreeMap;
use std::sync::Arc;

use polis_events::LogicalPath;

use crate::buildings::BuildingSpec;
use crate::geom::Pt;
use crate::{Building, LotId};

/// How many times the city's own count a cache may hold before it is emptied.
const SLACK: usize = 4;

/// FNV-1a's offset basis and prime, for the digests below.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// See [`FNV_OFFSET`].
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A digest under construction.
struct Digest(u64);

impl Digest {
    fn new() -> Self {
        Self(FNV_OFFSET)
    }

    fn word(&mut self, x: u64) -> &mut Self {
        self.0 ^= x;
        self.0 = self.0.wrapping_mul(FNV_PRIME);
        self
    }

    fn float(&mut self, x: f64) -> &mut Self {
        self.word(x.to_bits())
    }

    fn ring(&mut self, ring: &[Pt]) -> &mut Self {
        self.word(ring.len() as u64);
        for p in ring {
            self.float(p[0]);
            self.float(p[1]);
        }
        self
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Bit-for-bit ring equality. Not `==`: `-0.0 == 0.0` is true and they are not
/// the same input.
fn same_ring(a: &[Pt], b: &[Pt]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(p, q)| p[0].to_bits() == q[0].to_bits() && p[1].to_bits() == q[1].to_bits())
}

/// Bit-for-bit slice equality. See [`same_ring`].
fn same_floats(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

/// Hits and misses since a cache was last cleared.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    /// Results served without recomputation.
    pub(crate) hits: usize,
    /// Results computed because the cache did not have them.
    pub(crate) misses: usize,
}

impl Counts {
    /// The share served from the cache, in `[0, 1]`. Zero when nothing was asked
    /// for.
    #[allow(clippy::cast_precision_loss)] // two counts of one city's parts
    pub(crate) fn hit_rate(self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }
}

// ---------------------------------------------------------------------------
// Block parcellings
// ---------------------------------------------------------------------------

/// One block cut into parcels, sorted, with each buildable parcel's proof that
/// it clears the road.
///
/// # Why the cached value is this and not the raw subdivision
///
/// It used to be `crate::lots::subdivide_weighted`'s bare rings, and that put
/// the cache boundary in the wrong place. Measured at 5 000 files, one growth
/// step: the subdivision itself is **0.4 ms** and everything between it and the
/// occupant assignment — probing every ring for an interior point clear of the
/// carriageway, splitting the roomiest again until the block has a parcel per
/// file, and the frontage sort — is **7.1 ms**, on *every* step, cache hit or
/// miss, because none of it was remembered. That is a fifth of PRD §13.1's
/// whole incremental budget spent re-deriving parcels that had not moved.
///
/// Every one of those stages is a pure function of the same arguments the
/// subdivision already keys on, plus the number of files the block has to seat,
/// so moving the boundary down to here costs one extra word in the key. What is
/// deliberately left *outside* is the occupant assignment and the surplus
/// splitting: those read the files' identities, and a file arriving is exactly
/// the change a growth step is recording.
#[derive(Debug, Clone, Default)]
pub(crate) struct BlockCut {
    /// Parcels that can carry a building, in frontage order, each with the
    /// interior point that proved it clear of the road corridor and that
    /// point's clearance.
    pub(crate) viable: Vec<(Vec<Pt>, Pt, f64)>,
    /// The rest, in a canonical order. Vacant ground (PRD §7.5).
    pub(crate) spare: Vec<Vec<Pt>>,
}

/// `crate::lots::parcel_geometry`'s results, keyed on the whole of its input.
#[derive(Debug, Clone, Default)]
pub(crate) struct CutCache {
    entries: BTreeMap<u64, Vec<CachedCut>>,
    counts: Counts,
}

/// One remembered parcelling, with the whole of its input beside it.
#[derive(Debug, Clone)]
struct CachedCut {
    ring: Vec<Pt>,
    items: Vec<f64>,
    seed: u64,
    /// How many files the block had to seat: the densify loop's budget and its
    /// stopping condition, so two blocks that agree on everything else and not
    /// on this are two different parcellings.
    files: usize,
    /// `min_w`, `road_half`, `min_clear`, as bits.
    lengths: [u64; 3],
    cut: BlockCut,
}

impl CutCache {
    /// Hits and misses since the cache was last cleared.
    pub(crate) fn counts(&self) -> Counts {
        self.counts
    }

    /// Empties the cache when it has outgrown the city.
    pub(crate) fn trim(&mut self, blocks: usize) {
        if self.entries.len() > blocks.saturating_mul(SLACK) + SLACK {
            *self = Self::default();
        }
    }

    /// The parcelling for these exact arguments, computing it only when it is
    /// not already known.
    pub(crate) fn cut(
        &mut self,
        ring: &[Pt],
        items: &[f64],
        seed: u64,
        files: usize,
        lengths: [f64; 3],
        compute: impl FnOnce() -> BlockCut,
    ) -> BlockCut {
        let bits = [
            lengths[0].to_bits(),
            lengths[1].to_bits(),
            lengths[2].to_bits(),
        ];
        let mut d = Digest::new();
        d.word(seed).ring(ring);
        d.word(files as u64);
        d.word(items.len() as u64);
        for w in items {
            d.float(*w);
        }
        for l in bits {
            d.word(l);
        }
        let key = d.finish();

        if let Some(bucket) = self.entries.get(&key) {
            for entry in bucket {
                if entry.seed == seed
                    && entry.files == files
                    && entry.lengths == bits
                    && same_ring(&entry.ring, ring)
                    && same_floats(&entry.items, items)
                {
                    self.counts.hits += 1;
                    return entry.cut.clone();
                }
            }
        }
        self.counts.misses += 1;
        let cut = compute();
        self.entries.entry(key).or_default().push(CachedCut {
            ring: ring.to_vec(),
            items: items.to_vec(),
            seed,
            files,
            lengths: bits,
            cut: cut.clone(),
        });
        cut
    }
}

// ---------------------------------------------------------------------------
// Seated buildings
// ---------------------------------------------------------------------------

/// Everything `crate::buildings::place_in_parcel` reads.
///
/// Built for every parcel on every assembly — it is a handful of clones and two
/// ring copies against a footprint search — and then either found in the cache
/// or handed to the seating function.
#[derive(Debug, Clone)]
pub(crate) struct SeatKey {
    /// The parcel ring the building is inset from.
    pub(crate) parcel: Vec<Pt>,
    /// The block ring, which is the road centre line. Shared: a block has many
    /// parcels and copying its ring into each of their keys was measurably more
    /// expensive than the seating it was meant to save.
    pub(crate) block: Arc<Vec<Pt>>,
    /// [`digest_ring`] of `block`, computed **once per block** rather than once
    /// per parcel. A block ring is the longest thing in the key, and a block has
    /// several parcels.
    pub(crate) block_digest: u64,
    /// The occupant. Its bytes are the seed of every draw (PRD §7.4).
    pub(crate) path: LogicalPath,
    /// Size, height, class and the size reference.
    pub(crate) spec: BuildingSpec,
    /// The lot the building will carry.
    ///
    /// Deliberately **not** part of the key. `place_in_parcel` stores the id on
    /// the building and never reads it: the footprint comes from the two rings,
    /// the spec and a rotation seeded from the path. One new plot adds a parcel
    /// and renumbers every lot after it, so keying on the id would miss on
    /// roughly half the city for a label. A hit therefore carries the stored
    /// geometry with **this** assembly's id stamped on it — see
    /// [`SeatCache::get`].
    pub(crate) lot: LotId,
    /// The road corridor half-width.
    pub(crate) road_half: f64,
}

impl SeatKey {
    /// The lookup digest: everything the seating function reads, with the block
    /// ring folded in through the digest computed once for the whole block.
    fn digest(&self) -> u64 {
        let mut d = Digest::new();
        d.word(self.block_digest)
            .ring(&self.parcel)
            .word(self.spec.size_bytes)
            .word(u64::from(self.spec.diff_lines))
            .word(u64::from(self.spec.ghost_lines.to_bits()))
            .word(self.spec.class as u64)
            .word(self.spec.size_reference)
            .float(self.road_half);
        for b in self.path.as_str().as_bytes() {
            d.word(u64::from(*b));
        }
        d.finish()
    }

    /// Bit-for-bit equality of every argument the seating function reads.
    fn same(&self, other: &Self) -> bool {
        self.path == other.path
            && self.road_half.to_bits() == other.road_half.to_bits()
            && self.spec.size_bytes == other.spec.size_bytes
            && self.spec.diff_lines == other.spec.diff_lines
            && self.spec.ghost_lines.to_bits() == other.spec.ghost_lines.to_bits()
            && self.spec.class == other.spec.class
            && self.spec.size_reference == other.spec.size_reference
            && same_ring(&self.parcel, other.parcel.as_slice())
            && (Arc::ptr_eq(&self.block, &other.block) || same_ring(&self.block, &other.block))
    }
}

/// The digest of one block ring, for [`SeatKey::block_digest`].
pub(crate) fn digest_ring(ring: &[Pt]) -> u64 {
    let mut d = Digest::new();
    d.ring(ring);
    d.finish()
}

/// `crate::buildings::place_in_parcel`'s results from the **previous assembly**.
///
/// # An open-addressed table, written out
///
/// Three lookup structures were measured against the 4.4 ms (parallel) / 30 ms
/// (sequential) of seating they exist to skip, at 5 000 files:
///
/// | structure | hit rate | overhead per assembly |
/// |---|---|---|
/// | `Vec` indexed by [`LotId`] | ~60 % | ~0, and useless: one new plot adds a parcel and **every lot after it renumbers** |
/// | `BTreeMap` on the digest | 99 % | ~6 ms — a 5 474-node tree, allocated fresh every step |
/// | this: a flat probe table | 99 % | ~1 ms |
///
/// It is written out rather than reached for from `std` because a `HashMap` in
/// this crate is forbidden — `m1_gate::no_hash_map_reaches_the_layout` asserts
/// that structurally against the source, and rightly, since `RandomState` is
/// seeded per process. This table is never iterated, so its order could not
/// reach an output in any case; but "it would have been fine here" is not a rule
/// anyone can check, and a written-out table costs twenty lines.
///
/// # Bounded by construction
///
/// Rebuilt from scratch on every assembly, so it holds exactly one city's
/// answers and needs no eviction policy at all.
#[derive(Debug, Clone, Default)]
pub(crate) struct SeatCache {
    /// One entry per *occupied* parcel of the previous assembly, in lot order.
    entries: Vec<(SeatKey, Option<Building>)>,
    /// Slot to `entry index + 1`; `0` is empty. Always a power of two.
    slots: Vec<u32>,
    counts: Counts,
}

impl SeatCache {
    /// Hits and misses since the cache was built.
    pub(crate) fn counts(&self) -> Counts {
        self.counts
    }

    /// A table sized for `parcels` entries at a load factor of one half, so a
    /// probe walks a couple of slots at worst.
    pub(crate) fn with_capacity(parcels: usize) -> Self {
        let slots = parcels.saturating_mul(2).max(16).next_power_of_two();
        Self {
            entries: Vec::with_capacity(parcels),
            slots: vec![0; slots],
            counts: Counts::default(),
        }
    }

    /// First slot to probe for `digest`.
    ///
    /// The high half: FNV-1a mixes upward, so its top bits are the ones that
    /// actually vary.
    fn slot_of(&self, digest: u64) -> usize {
        (digest >> 32) as usize & (self.slots.len() - 1)
    }

    /// The building the previous assembly seated on this exact parcel.
    ///
    /// `&self`, so the whole parcel list is looked up before the misses are
    /// seated — which is what lets the misses stay a `rayon` `par_iter` while
    /// the cache is touched from one thread only.
    ///
    /// A probe that reaches an empty slot is a miss. A probe that finds an entry
    /// whose arguments are not bit-for-bit these ones keeps walking: a digest
    /// collision costs a few more probes, never a wrong building.
    // The three cases are all real and all distinct: `None` is "not remembered",
    // `Some(None)` is "remembered, and this parcel cannot hold a building" — the
    // most expensive answer the seating function gives — and `Some(Some(b))` is
    // the building. Collapsing the first two would re-run the exhaustive search
    // on every unbuildable parcel of every growth step.
    #[allow(clippy::option_option)]
    pub(crate) fn get(&self, key: &SeatKey) -> Option<Option<Building>> {
        if self.slots.is_empty() {
            return None;
        }
        let mut slot = self.slot_of(key.digest());
        for _ in 0..self.slots.len() {
            let at = self.slots[slot];
            if at == 0 {
                return None;
            }
            let (stored, building) = &self.entries[at as usize - 1];
            if stored.same(key) {
                // The id is a label, not geometry: stamp this assembly's on the
                // remembered footprint. See [`SeatKey::lot`].
                let mut building = building.clone();
                if let Some(b) = building.as_mut() {
                    b.lot = key.lot;
                }
                return Some(building);
            }
            slot = (slot + 1) & (self.slots.len() - 1);
        }
        None
    }

    /// Records one occupied parcel's answer, hit or miss.
    ///
    /// The key is **moved**: the caller has just finished with it.
    pub(crate) fn put(&mut self, key: SeatKey, building: Option<Building>, hit: bool) {
        if hit {
            self.counts.hits += 1;
        } else {
            self.counts.misses += 1;
        }
        // Sized for the parcel count at a load factor of one half; a caller that
        // pushes more than it asked for must still be correct rather than fast.
        if (self.entries.len() + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let digest = key.digest();
        self.entries.push((key, building));
        let index = u32::try_from(self.entries.len()).expect("parcel count fits in u32");
        let mut slot = self.slot_of(digest);
        while self.slots[slot] != 0 {
            slot = (slot + 1) & (self.slots.len() - 1);
        }
        self.slots[slot] = index;
    }

    /// Doubles the table and re-inserts every entry.
    fn grow(&mut self) {
        self.slots = vec![0; self.slots.len().max(8) * 2];
        for i in 0..self.entries.len() {
            let digest = self.entries[i].0.digest();
            let mut slot = self.slot_of(digest);
            while self.slots[slot] != 0 {
                slot = (slot + 1) & (self.slots.len() - 1);
            }
            self.slots[slot] = u32::try_from(i + 1).expect("parcel count fits in u32");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(scale: f64) -> Vec<Pt> {
        vec![[0.0, 0.0], [scale, 0.0], [scale, scale], [0.0, scale]]
    }

    /// A parcelling of one ring, as the cache stores one.
    fn parcelling(scale: f64) -> BlockCut {
        BlockCut {
            viable: vec![(ring(scale), [scale * 0.5, scale * 0.5], scale)],
            spare: Vec::new(),
        }
    }

    /// One argument list for [`CutCache::cut`]: ring, weights, seed, file count,
    /// lengths.
    type Variant = (Vec<Pt>, Vec<f64>, u64, usize, [f64; 3]);

    /// A hit returns the stored value and does not run the computation again.
    #[test]
    fn an_identical_cut_is_not_recomputed() {
        let mut cache = CutCache::default();
        let items = vec![1.0, 2.0];
        let first = cache.cut(&ring(1.0), &items, 7, 2, [0.1, 0.2, 0.3], || {
            parcelling(0.5)
        });
        let second = cache.cut(&ring(1.0), &items, 7, 2, [0.1, 0.2, 0.3], || {
            panic!("the cut was recomputed for identical arguments")
        });
        assert!(same_ring(&first.viable[0].0, &second.viable[0].0));
        assert_eq!(cache.counts(), Counts { hits: 1, misses: 1 });
        assert!((cache.counts().hit_rate() - 0.5).abs() < f64::EPSILON);
    }

    /// Every argument is part of the key: change any one and the cut is
    /// recomputed. This is the test that stops the cache from becoming a source
    /// of stale geometry the moment someone adds an input to the subdivision.
    #[test]
    fn every_argument_is_part_of_the_key() {
        let base = (ring(1.0), vec![1.0, 2.0], 7u64, 2usize, [0.1, 0.2, 0.3]);
        let variants: Vec<Variant> = vec![
            (ring(1.5), base.1.clone(), base.2, base.3, base.4),
            (base.0.clone(), vec![1.0, 2.5], base.2, base.3, base.4),
            (base.0.clone(), vec![1.0, 2.0, 3.0], base.2, base.3, base.4),
            (base.0.clone(), base.1.clone(), 8, base.3, base.4),
            (base.0.clone(), base.1.clone(), base.2, 3, base.4),
            (
                base.0.clone(),
                base.1.clone(),
                base.2,
                base.3,
                [0.9, 0.2, 0.3],
            ),
            (
                base.0.clone(),
                base.1.clone(),
                base.2,
                base.3,
                [0.1, 0.9, 0.3],
            ),
            (
                base.0.clone(),
                base.1.clone(),
                base.2,
                base.3,
                [0.1, 0.2, 0.9],
            ),
        ];
        for (i, v) in variants.iter().enumerate() {
            let mut cache = CutCache::default();
            cache.cut(&base.0, &base.1, base.2, base.3, base.4, || parcelling(0.5));
            let mut ran = false;
            cache.cut(&v.0, &v.1, v.2, v.3, v.4, || {
                ran = true;
                parcelling(0.25)
            });
            assert!(ran, "variant {i} was served from the cache");
        }
    }

    /// `-0.0` is not `0.0` as an input, however `==` feels about it.
    #[test]
    fn negative_zero_is_a_different_ring() {
        let mut cache = CutCache::default();
        let a = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]];
        let b = vec![[-0.0, 0.0], [1.0, 0.0], [1.0, 1.0]];
        cache.cut(&a, &[1.0], 1, 1, [0.0, 0.0, 0.0], || parcelling(1.0));
        let mut ran = false;
        cache.cut(&b, &[1.0], 1, 1, [0.0, 0.0, 0.0], || {
            ran = true;
            parcelling(2.0)
        });
        assert!(ran, "-0.0 was served the cut for 0.0");
    }

    /// The cache is bounded: it is emptied once it holds more than the city.
    #[test]
    fn the_cache_does_not_grow_without_bound() {
        let mut cache = CutCache::default();
        for i in 0..200u64 {
            cache.trim(10);
            #[allow(clippy::cast_precision_loss)] // a loop counter
            let r = ring(1.0 + i as f64);
            cache.cut(&r, &[1.0], i, 1, [0.0, 0.0, 0.0], || BlockCut {
                viable: vec![(r.clone(), [0.0, 0.0], 1.0)],
                spare: Vec::new(),
            });
        }
        assert!(
            cache.entries.len() <= 10 * SLACK + SLACK + 1,
            "the cache held {} entries for a ten-block city",
            cache.entries.len()
        );
    }
}
