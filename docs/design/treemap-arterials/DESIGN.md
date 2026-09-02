# treemap-arterials

**District borders *are* the arterial roads.** The road network is not grown and
then hoped to close — it is a space-filling subdivision of the plane over the
directory tree, so it is connected, planar, and made entirely of closed faces
from the first split onward.

Renders in this directory:

| File | What it shows |
|---|---|
| `small.png` | the real `agentolis` repository, 86 files |
| `large.png` | a synthetic 5000-file monorepo |
| `junctions.png` | road graph only, small repo, junctions coloured by degree |
| `junctions-large.png` | the same for the 5000-file repo |
| `detail-small.png`, `detail-large.png` | zoomed crops, so the fabric can be judged rather than guessed at |
| `metrics.txt` | the measured numbers below, as emitted by the program |
| `digest-*.txt` | the canonical layout dump used for the byte-identity proof |

Prototype source: `prototype/` (also at
`…/scratchpad/design-treemap-arterials/`). `cargo run --release [outdir]`.

---

## 1. Why the M1 attempt failed, and what this changes

Space colonization is a tree-growing algorithm. Its output is a tree; PRD §7.2
says so, and says snapping is what introduces cycles. But snapping can only
bridge things that are *already near each other*, and golden-angle district
placement guarantees they are not — each attractor cloud is spatially isolated,
so every island grows its own little tree and the graph never closes. No cycles
→ no faces → no blocks → degenerate lots → degenerate buildings. The whole
downstream pipeline inherits one topological failure at stage 2.

This design inverts the dependency. Instead of hoping a growth process closes,
it starts from something that is closed and subdivides it. A closed convex
polygon cut by a chord yields two closed polygons. Recursively: *every*
intermediate state is a connected planar graph whose bounded faces tile the
original polygon exactly. Cycles are not an emergent property to be coaxed out;
they are the invariant.

The PRD ordering is preserved, not skipped: roads → blocks → lots → buildings.
Blocks are still the closed faces of the road graph (§7.2 stage 3). It is just
that the road graph is constructed face-first rather than segment-first.

---

## 2. Architecture

### 2.1 One arrangement, six passes

Everything lives in a single planar arrangement (`arr.rs`): `nodes: Vec<Pt>`,
`faces: Vec<ring of node ids>`, `edges: BTreeMap<(u32,u32), {faces, class}>`.
One operation does all the work:

```
split_face(f, line, snap_r, class) -> (face_a, face_b)
```

It finds the line's two crossings of `f`'s ring, materialises them as nodes, and
splits the ring. Crucially, when a crossing point falls on the interior of an
edge, `split_edge` inserts the new vertex into **every** face that uses that
edge. That is why two districts on opposite sides of a border keep an
*exactly* shared boundary no matter how much either side is subdivided later —
the shared border is one edge chain, not two coincident polylines.

1. **City rim.** A wobbled 19-gon, then its **convex hull**. Convexity of the
   root is the load-bearing invariant: chord-splitting a convex polygon yields
   two convex polygons, and a chord of a convex polygon cannot leave it. That
   single property is what makes the whole subdivision planar by construction
   rather than by hope. (I learned this the hard way — the first version used
   the raw wobbled polygon, which is sometimes non-convex, and the resulting
   arrangement had road crossings.)

2. **Districts (arterials).** Recursive binary partition of the directory tree.
   At each directory the children are ordered by `(oldest file, path)` and split
   at the index that best balances quantised subtree weight; the face is cut with
   `offset_for_area_ratio` so each half gets area proportional to its file count.
   Split *direction* is perpendicular to the face's longest axis, leaned 30%
   toward the terrain gradient (roads follow contours), then rotated by a
   seeded jitter of ±0.09 rad in new districts and ±0.61 rad in the oldest. A
   bounded retry ladder (9 angles, shrinking snap radius) guarantees termination.

3. **Blocks (streets).** The same operation again *inside* each district face,
   until the block count matches both the district's file count and its ground
   area. Because it is the same machinery on the same arrangement, block edges
   that touch a district border are the border — no stitching, no seams.

4. **Dissolve.** A seeded fraction of interior block edges is deleted (22% in the
   oldest districts, 7% in the newest), merging the two faces into one larger,
   *non-convex* block. Edges are only removed when both endpoints keep degree
   ≥ 3, so the graph keeps minimum degree 2 and there are **zero dangling road
   stubs**. This is where the irregular, many-sided blocks of an old town come
   from.

5. **Curve and warp.** Every edge is subdivided to ~7 units, then every node is
   displaced by one smooth vector field. Because the displacement is a pure
   function of position, both sides of a shared border move together and the
   tiling survives exactly. Three octaves: two at lot scale (age-modulated —
   old districts get 2.3× the amplitude of new ones), and one at half the city
   radius that bends the long arterials. Amplitude/wavelength is kept low enough
   that the map is a diffeomorphism; measured crossings are 0 at both scales.

6. **Extract.** Roads = all arrangement edges. Blocks = all live faces. A
   segment is an **arterial iff its two adjacent faces belong to different
   districts** — the district boundary set is read straight off the tiling, not
   reconstructed.

### 2.2 Lots and buildings

Blocks are inset **per edge** by the half-width of the road that actually runs
along that edge (a boulevard costs more ground than an alley), after simplifying
the ring so the warp's waviness does not collapse the inset to the block's
convex kernel.

Lots then use a **street-fronting perimeter ring**, not a chord fan: the block is
paired with a miter inset of itself, and the annulus between them decomposes
exactly into one trapezoid per edge, apportioned by frontage length. This is the
single change that made the render stop looking like shattered glass and start
looking like frontages. The enclosed courtyard becomes a garden. Blocks with
fewer than 5 lots skip the ring and use PRD §7.2's plain recursive longest-axis
subdivision, because a block with three lots has no business having a courtyard.
695 of 707 blocks take the ring path at 5000 files.

Buildings are lots inset by a setback, scaled so footprint ∝ √bytes (clamped
into the lot), rotated ±4°, with a containment check that shrinks until the
polygon is verifiably inside its lot.

### 2.3 Streets (PRD §9)

Cross-district import edges are aggregated per district pair and routed with
Dijkstra **over the road graph**. A street is a path along existing roads; it is
never a chord. Measured: 70 routed, 0 chord-like at 5000 files.

---

## 3. Where the cycles and the enclosed blocks come from

There is nothing to argue: for a connected planar graph, independent cycles =
E − V + C. Every split adds one chord (one edge) and at most two nodes that are
themselves edge subdivisions (net V unchanged by a subdivision, +1 edge). Each
successful `split_face` therefore adds exactly one independent cycle, and it
adds exactly one bounded face. Blocks and cycles are the same object counted two
ways: **709 blocks, 709 cycles** at 5000 files; **27 and 27** at 86 files.

The organic signature — 4- and 5-way junctions — comes from PRD §7.2's snapping,
applied at construction instead of after the fact: when a chord endpoint lands
within `snap_r` of an existing ring vertex it *reuses* that vertex. Two chords
sharing an endpoint make a degree-4 node; three make a degree-5. Without it the
graph would be pure degree-3 T-junctions, which reads as a slime mould. With it,
**39% of junctions are degree ≥ 4** at 5000 files.

A guard I did not anticipate needing: when *both* chord endpoints snap onto the
same straight stretch of boundary, the split produces a collinear zero-area face
and leaves two coincident paths between the same node pair — the arrangement
silently stops being planar. Rejecting any split whose parts are below 2% of the
parent's area fixes it, and the retry ladder finds another angle. That one guard
took the 5000-file crossing count from 22 to 0.

---

## 4. Determinism (PRD §7.4)

Reuses `polis_layout::determinism` throughout: `SeededRng::for_path` /
`for_path_indexed` / `for_seed`, the crate's own simplex/fbm, `det_sin_cos` for
every rotation, `fnv1a64` for digests. No wall clock, no global RNG, no
`DefaultHasher`, no `HashMap` on any path that reaches geometry — the
arrangement's edge table is a `BTreeMap<(u32,u32), _>` and every per-district
collection is a `BTreeMap` or a sorted `Vec`.

Beyond reuse, three properties of the algorithm itself matter:

* **All layout arithmetic is `+ - * /` and `sqrt`**, which IEEE-754 requires to
  be exactly rounded, so it agrees bit-for-bit across machines. The only
  transcendentals are the noise (written out in `determinism.rs`) and rotations
  (through `det_sin_cos`, grid-quantised).
* **`offset_for_area_ratio` is a fixed 44-iteration bisection**, not a
  convergence loop — no `while |err| > eps` whose iteration count could differ.
* **Nothing depends on iteration order.** Rebuilding both `RepoTree`s with the
  files inserted in reverse order produces an identical digest (`matches: true`
  for both scales, printed in `metrics.txt`).

**Proof, two separate processes, same binary, different output directories:**

```
$ ./treemap-arterials runA ; ./treemap-arterials runB
$ diff -r --brief runA runB
Files runA/metrics.txt and runB/metrics.txt differ      <- timing lines only
Files runA/stdout.txt  and runB/stdout.txt  differ      <- same
```

Every geometry artefact is byte-identical:

```
small.png            f8642b998e4da034ec71fc465cc808f019b344075bb6924453c65e3d14fdd05e  MATCH
large.png            2675341daa14b657914d9ac912f4d58c286acbca1db4f648689f90041ac801c5  MATCH
junctions.png        127e2f19b734d7222a33b8d954ccc059bf78c4848522643f3f207ce749e43181  MATCH
junctions-large.png  56a4a72e036bec47e9e0e3742248c5ea55f1af6b49ceb660b488d9caa032c526  MATCH
detail-small.png     a5b5d37b9bc0aad22cd8b8b5a3247411bb398e90f7fe6f30a78ef3ad8da7c9c8  MATCH
detail-large.png     f8695caa32eab39756dbad0bea6d4362050cb6c084c3aed049243851e418ea1a  MATCH
digest-small.txt     9b3aa4391da9713d2c56156b52b3ea8f1f9c0677da7aa064e0707f73e80ecfe5  MATCH
digest-large.txt     be7a928d98a2628a675237e9bd46a22523e1d871ce45e5c43bdb9445d268d5ad  MATCH
```

The only diff in `metrics.txt` is the measured wall-clock line
(`incremental_add(vacant lot)=0.6us` vs `0.3us`) — a timing report, not an input.

The renderer is deterministic too (the disc rasteriser uses a constant unit-vector
table rather than `sin`/`cos`), which is why the PNGs and not merely the layouts
hash equal.

*(A trap worth recording: the prototype writes its renders into `docs/design/`,
inside the repository it renders. The walk picked them up and the city grew by a
few files every run. Generated artefacts are now excluded from the walk. Any real
implementation has the same exposure through `.gitignore`d build output.)*

---

## 5. Incremental growth (PRD §7.4, §7.7)

Two properties, both structural rather than best-effort:

**A district's outer polygon is fixed by its parent.** Re-subdividing a district
changes nothing outside its own boundary — not one neighbouring block, not one
road. This is much stronger than anything a global growth process can offer, and
it is what makes "never move the ground while the operator is looking at it"
achievable rather than aspirational.

**Weights are quantised, and lots are surveyed with headroom.** A district
holding *n* files is given `lots_for(n) = quant(n) + max(2, quant(n)/6)` lots,
where `quant` rounds up to 5 significant binary digits. Adding one file almost
always leaves `lots_for` unchanged, so the new file simply occupies a lot that
was already surveyed. The spare lots are not waste — PRD §7.5 wants vacant lots
on the map anyway, and they are drawn as such.

So the growth step is:

* **Common case** — bucket unchanged: find the district, take its first vacant
  lot, build. Measured **1.3 µs** at 5000 files. Nothing moves.
* **Bucket boundary crossed**: re-subdivide that district's interior and
  re-parcel it, inside its unchanged outer polygon. Measured **0.51 ms** at
  5000 files (the largest district), against a 50 ms budget.
* **A new directory appears**: its parent's subtree re-splits. That does move
  ground, but only inside the parent's polygon, and it is exactly the moment a
  city visibly gains a neighbourhood.

Deletion is symmetric: the lot goes vacant (§7.5) and the skeleton does not move.

---

## 6. Making git age and the directory tree visible

**The tree determines placement, bindingly (PRD §9).** Position is a pure
function of the directory tree; imports never move anything. Directories that
are siblings get sub-faces of the same parent face, so they are adjacent on the
map and share a border. Adjacency is measured: at 5000 files, **97/97 districts
share a border with another and the district adjacency graph has exactly 1
connected component**. There are no island districts because the construction
cannot produce one.

**Git age is not decorative here — it is four separate levers (PRD §7.1):**

1. *Position.* At every split the older group is given the sub-face nearer the
   civic square (the normal is oriented so the `≤ s` half is the origin-ward
   one). Old subtrees migrate to the centre; recent ones end up on the rim.
2. *Irregularity.* Split-angle jitter runs ±0.09 rad in the newest districts and
   ±0.61 rad in the oldest, and the split ratio spread goes from ±0.06 to ±0.26.
   The old town is visibly tangled; the periphery is visibly planned.
3. *Grain.* Old districts get more, smaller blocks (`lots_per_block` scales down
   with age) and 22% edge dissolution against 7% — dense irregular fabric versus
   large regular blocks.
4. *Warp.* Warp amplitude is multiplied by `0.45 + 1.25 × oldness`, sampled from
   a raster of district age, so streets bend more in the historic core.

Within a district, files are assigned to lots in `(growth_index, path)` order
and lots are ordered by distance from the civic square, so the age gradient is
legible at street level too.

**Wayfinding (PRD §8).** The arterial skeleton is exactly the district boundary
set, so the wayfinding layer and the road layer are the same data. At the widest
zoom you can draw only the arterials and get a complete, connected, legible
district diagram for free — no separate simplification pass, no risk of the
skeleton disagreeing with the streets. Folding directories under a size
threshold into their parent (§7 below) keeps that skeleton at ~97 districts for
5000 files instead of 628, which is the difference between a readable
neighbourhood map and a mosaic.

---

## 7. Measured metrics

Computed by `metrics.rs` from the generated layout; verbatim in `metrics.txt`.

### Small — the real `agentolis` repo, 86 files

| | |
|---|---|
| road graph | nodes 236, segments 262, **components 1**, **crossings 0**, **cycles (E−V+C) 27** |
| junction degrees | 2:197, 3:29, 4:8, 5:1, 6:1 — **dangling (deg 1): 0**, deg ≥ 4 = 25.6% of junctions |
| blocks | 27; area min/med/max 215 / 750 / 1617; aspect min/med/p95/max 1.09 / 1.53 / 2.46 / 2.76; **slivers 0**; median 18 vertices |
| buildings | lots 115, buildings 86, files 86, **without building 0**, **outside lot 0**, **intersecting a road 0** |
| districts | 16; **16 share a border**; adjacency components **1**; 29 border pairs |
| streets | 6 routed, **0 chord-like** |
| timing | full generation **0.8 ms**; incremental add (vacant lot) **1.4 µs**; incremental add (subtree re-subdivide) **0.04 ms** |

### Large — synthetic 5000-file monorepo (5007 files placed)

| | |
|---|---|
| road graph | nodes 7124, segments 7832, **components 1**, **crossings 0**, **cycles (E−V+C) 709** |
| junction degrees | 2:6300, 3:502, 4:168, 5:77, 6:51, 7:16, 8:8, 9:2 — **dangling (deg 1): 0**, deg ≥ 4 = **39.1%** of junctions |
| blocks | 709; area min/med/max 166 / 1543 / 7413; aspect min/med/p95/max 1.00 / 1.88 / 3.99 / 8.11; **slivers 4** (0.6%); median 21 vertices |
| buildings | lots 5902, buildings 5007, files 5007, **without building 0**, outside lot 8 (0.16%), intersecting a road 39 (0.8%) |
| districts | 97; **97 share a border**; adjacency components **1**; 193 border pairs |
| streets | 70 routed, **0 chord-like** |
| timing | full generation **33.6 ms** (budget 3 s); incremental add (vacant lot) **1.3 µs**; subtree re-subdivide **0.51 ms** (budget 50 ms) |

"Sliver" = OBB aspect > 6 **or** isoperimetric compactness < 0.16. The synthetic
repo has deep nesting (to 6 levels), two dominant trees (`src`, `packages`), a
vendored mass, and a long tail of 5–15-file directories; whole directories enter
the growth sequence together, as they do in a real history.

---

## 8. Honest critique of my own renders

**Look at `detail-large.png` first; the full-city views flatter the design.**

What works. It is unambiguously **one connected settlement**. There are no
islands, no dangling roads trailing into a void, no chords slashing the frame.
`junctions-large.png` settles the "is it a tree?" question in one glance: zero
red nodes, 322 green-and-yellow junctions, 709 cycles. Blocks are real enclosed
faces with sane aspect ratios and effectively no slivers. Districts tile the map
and adjacent directories are adjacent on the ground. At the widest zoom
`large.png` reads as a dense built-up area with a legible arterial armature and
coloured districts — from across the room it reads as an aerial of a city.

What does not work, plainly:

1. **`small.png` does not read as a city. It reads as a walled town plan, or a
   stained-glass panel.** 86 files over 16 districts is 27 blocks with three or
   four buildings each; road widths are absolute, so on a 190-unit-wide city the
   boulevards are proportionally enormous and dominate the image. The green
   garden/vacant ground outweighs the built ground. This is partly honest —
   a 90-file repo *is* a village — but the render leans much harder into
   "diagram" than into "city", and I would not defend it as reading as a city
   from across the room. The large render is the one that carries the claim.

2. **Courtyards are too big and the fabric is too thin.** Even at 5000 files a
   block averages ~7 buildings. A real block that size holds forty. The
   perimeter ring is the right structure, but with this building count it draws
   a thin frame around a large empty interior, which at mid zoom gives a
   "cell wall" texture rather than a built texture. Reducing land per file would
   help; I ran out of iterations to calibrate it properly.

3. **The arterials are still too straight, and that is the treemap showing
   through.** The first two or three splits are single chords crossing most of
   the city. The city-scale warp octave bends them, but only by ~4% of the
   radius, because more than that starts producing road crossings. Long straight
   boulevards meeting at a point are the mechanical tell the brief warned about,
   and it is still visible in `large.png` — look at the long warm lines running
   corner to corner.

4. **Junctions cluster into stars.** Because a chord endpoint snaps to a nearby
   vertex, several chords converge on the same node and you get 7-, 8- and
   9-way stars that look more like a roundabout diagram than a street corner.
   That is where most of the deg ≥ 4 count comes from, and it is a slightly
   dishonest way to earn that metric: it is one big junction rather than many
   irregular ones.

5. **Blocks are convex-ish and similar.** Only ~20% of block edges are dissolved,
   so most blocks are 4–6-gons of similar size. Real old towns have far wider
   variance in block size and shape than this produces.

6. **The chord-split fallback is visibly worse than the ring.** 12 of 707 blocks
   at 5000 files (and 2 of 27 at 86) fall back to plain longest-axis
   subdivision, and they show as triangular fans amid the frontages.

Verdict: **the topology problem is solved and the wayfinding structure is good;
the urban texture is mediocre and the small-repo render is weak.** If the gate's
question is "is this a connected, planar, cyclic city with real blocks and
legible districts", the answer is yes with numbers to back it. If the question is
"does it read as a city at 90 files", the answer is not yet.

---

## 9. Cost to land in `polis-layout`

**Reusable as-is:** `determinism.rs` entirely (this prototype uses nothing else
for randomness or noise); `lib.rs`'s `Point`/`Vec2`/`Polygon`/`RoadGraph`/
`Block`/`Lot`/`Building`/`District`/`CityLayout` vocabulary — the prototype's
output maps onto it one-to-one; `polis-repo`'s `RepoTree`, `DistrictTree`,
`GrowthSequence`, `entry_point`, `ImportGraph::cross_district_edges`.

**New file — `arrangement.rs`, ~420 lines.** The planar subdivision:
`split_face`, `split_edge`, `remove_edge`, `subdivide_edges`, `compact`,
`offset_for_area_ratio`. This is the whole idea and it has no counterpart in the
current tree. It needs unit tests for the invariants (every live face has ≥ 3
distinct vertices; every interior edge has exactly 2 faces; the sum of face
areas equals the root area).

**Rewritten — `roads.rs`.** `plan_districts`, `scatter_attractors` and the
colonization loop go away entirely; the golden-angle spiral is the bug. What
replaces them is the district-placement recursion (~200 lines) and the block
subdivision loop (~90). `RoadGraph::add_node_snapped` and `snap_target` survive
in spirit but move into the arrangement, where snapping is a construction-time
decision rather than a post-hoc repair.

**Rewritten — `blocks.rs`.** Face extraction from a planar graph is the single
biggest thing this deletes: blocks come out of the arrangement already. What
remains is classification (area, aspect, sliver rejection) and the dissolve pass
(~120 lines). I would expect `blocks.rs` to shrink substantially.

**Extended — `lots.rs`.** Keep the recursive longest-axis subdivision (PRD §7.2
step 4 — it is still the fallback and still correct). Add `perimeter_lots` and
`miter_inset_paired` (~150 lines), plus `simplify_with` and `inset_edges` as
geometry primitives in `lib.rs` (~90).

**Mostly unchanged — `buildings.rs`, `terrain.rs`.** Buildings need the
containment-verified inset (~40 lines changed). Terrain is already what this
design consumes: a gradient field to lean split normals into.

**Extended — `city.rs`.** The pipeline driver, plus the incremental API:
`lots_for`, `add_file`, `resubdivide_district`. The last is genuinely new and is
where the §7.7 guarantee lives.

**New — the streets router.** ~90 lines of Dijkstra over the road graph with a
quantised-cost heap for determinism.

Rough total: **~700 lines new, ~600 rewritten, ~500 deleted**, plus tests. Two to
three days. The two things I would insist on before merging: a golden-file test
(§16) pinned to the arrangement digest, and a debug assertion that counts
segment crossings after the warp — the warp amplitude is the one parameter that
can silently break planarity, and it is the first thing anyone will want to turn
up.

**Risks worth naming.** The convexity invariant is load-bearing and easy to break
by accident (a well-meaning "make the rim more interesting" commit is enough).
The degenerate-split guard is the kind of thing that gets refactored away by
someone who does not know why it is there. And the fundamental limit stands: a
subdivision has as many top-level chords as the tree has top-level splits, and
those chords are straight before the warp. If the judgement is that the result
still reads as mechanical, no amount of warping fixes it — the answer would be to
keep this arrangement as the *arterial skeleton only* and grow the interior
fabric by some other means, using the guaranteed-closed district faces as the
containers that space colonization was never able to produce for itself.
