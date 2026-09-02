# voronoi-organic — districts as an organic partition of the plane

**Renders (all 1800 × 2010, the bottom 210 px is a metrics strip):**

| file | what |
|---|---|
| `small.png` | the real `agentolis` repo, 86 files |
| `large.png` | a synthetic 5 122-file repo, deep nesting, uneven directory sizes |
| `junctions.png` | small: road graph alone, junctions coloured by degree |
| `junctions-large.png` | same for the 5 k repo |
| `metrics.txt` | the numbers below, as emitted by the prototype |
| `prototype-src/` | the working prototype, 4 406 lines of Rust, no dependencies beyond `polis-layout` / `polis-repo` |

---

## 1. The one-sentence idea

**Do not grow roads. Grow *ground*, and let the roads be its seams.**

The failed M1 implementation grew a road network into empty space with space
colonization and hoped cycles would appear. They didn't, because space
colonization is a tree-growing algorithm (PRD §7.2 says so outright), and
snapping only closes loops when attractor clouds overlap — which they never did,
because the districts were scattered on a golden-angle spiral.

This design inverts the dependency. It partitions the settled ground into
regions first, and then **defines the road network as the boundary of that
partition**. A partition of a connected region has, by construction:

* a **connected** boundary graph — the seams of a jigsaw are one piece;
* a **planar** boundary graph — it comes out of a raster crack graph, where two
  boundaries cannot cross without meeting at a lattice vertex;
* one **closed face per region** — Euler's formula gives you `F − 1` bounded
  faces for free, so "blocks are the closed loops in the road graph" is a
  theorem here rather than a hope.

Islands, dangling stubs, chords across open space and sliver blocks are not
bugs that were fixed. They are unrepresentable.

---

## 2. Architecture

The PRD §7.2 ordering is preserved exactly: **terrain → roads → blocks → lots →
buildings.** What changes is how roads are obtained.

### 2.1 Terrain field (PRD §7.2 stage 1)

Two things, both from `polis_layout::determinism`'s own simplex/fBm (never the
`noise` crate — ADR-0050):

1. **A domain warp.** Every point `p` is displaced to
   `p' = p + 52·fbm(p/235) + 9.5·fbm(p/61)` before anything looks at it. The
   whole partition is computed in warped space. Because the warp is a single
   smooth near-isometry applied to *every* query, the hierarchy stays nested and
   the cells stay connected — but every border acquires a coherent wander at two
   scales. This is what stops the map reading as crystalline. Straight-edged
   Voronoi is the failure mode the brief warned about; the warp is the whole
   answer to it.
2. **A coastline and a town outline.** `land(p')` is a radial falloff plus fBm;
   `settled(p')` is a second, tighter falloff with a different seed. The largest
   connected land mass survives, interior pockets are filled (a hole inside the
   town would leave an orphan ring in the road graph), and the ring between the
   two curves is countryside.

The terrain is never drawn. Its only job is to justify curvature.

### 2.2 The directory tree splits the ground recursively (PRD §9, binding)

The augmented tree is the directory tree plus, for every directory that holds
files directly, a **self-district** child (`dir/·`) inserted *first* among its
children — so a directory's own files form the core of its quarter and its
subdirectories ring it.

The root owns every settled pixel. Then, repeatedly:

1. Compute the region's centroid `c` and effective radius `r = sqrt(area/π)`.
2. Give each child a **target** position: the self-district at `c`; each
   subdirectory in an angular wedge sized by `qcount^0.55`, allocated in
   canonical key order (so alphabetically adjacent siblings end up adjacent on
   the map), at offset `r·(0.46 + 0.30·age_rank)` with a seeded ±30 % jitter.
3. **Project each target onto the parent's own pixel set** — the nearest pixel
   the parent actually owns becomes the child's site.
4. Partition the parent's pixels by nearest site under the multiplicatively
   weighted metric `d(p,s) = |p−s|² / m_s`.

Step 3 is the load-bearing one. It is why no cell can be empty (a site is a
pixel of its own parent, and `d(s,s)=0`), and why children are always inside
their parent (containment is a property of the walk, not of the geometry). The
first version of this prototype placed sites by formula and partitioned
globally; at 5 000 files **506 of 908 districts came out with zero pixels**.
Projection took that to zero.

`m_s = (quantised subtree file count)^0.85`, normalised **per sibling group**
(comparisons are only ever sibling-local) with a floor of 1/110. Because
`d` is divided by `m`, the boundary between two sites is an Apollonius circle,
not a straight bisector — so cell area scales roughly with file count *and* the
borders are already curved before the terrain warp touches them. The floor is a
deliberate departure from strict proportionality: a 3-file directory beside a
1 400-file one would otherwise be sub-pixel, and a quarter you cannot see is a
quarter you cannot navigate to.

**Splitting stops** when a region can no longer carry a legible quarter
(`< children × 1700` cells). Below that scale the whole subtree becomes one
district. This is semantic zoom, and it is why the 5 k repo shows 494 districts
rather than 908 unreadable specks.

### 2.3 Blocks: one more level of the same partition

Inside each district, `n` block seeds are dart-thrown **from the district's own
pixel list** (blue-noise, seeded, min-distance relaxed on failure), ordered by
distance from the district core, and the district's pixels are re-partitioned by
plain nearest-seed. Block count is area-driven —
`round(cells / (1500 · (0.45 + 1.20·age_rank)))`, capped by `files/2.2` and by a
minimum viable block size — so the street grain is the same density at 86 files
and at 5 122.

That age term is PRD §7.1 made structural rather than decorative: **an old
district's blocks are ~2.7× smaller than a new one's**, so the historic core is
visibly finer-grained and more tangled while the periphery is coarse and planned.

### 2.4 Roads = the seams (PRD §7.2 stage 2)

The block labelling is a raster. Its **crack graph** — the lattice edges between
differently-labelled pixels — is the road network.

* Lattice vertex degree is 0, 2, 3 or 4 by construction. Degree ≠ 2 vertices are
  junctions; the degree-2 runs between them are traced into **chains** carrying
  their two flanking labels.
* Chains are Douglas-Peucker simplified (tol = 2 raster cells) to remove the
  staircase, then **short chains are collapsed**: any chain shorter than
  0.34 × median is contracted, merging its two endpoints into one node.

That collapse is this design's version of PRD §7.2's snapping rule, and it does
two jobs at once. Contracting an edge between two degree-3 nodes yields **one
degree-4 node**; contracting two adjacent ones yields degree 5. It also deletes
the sliver face that sat between them. A guard refuses to take any face below
three chains, so no block is destroyed. The result is the measured 27–29 % of
junctions at degree 4 or above, with **zero dangling ends**.

Finally two Chaikin passes turn the simplified chains into curves.

Classification is free: a chain between two blocks of *different* districts is an
**arterial**; same district, a **secondary street**; against unsettled ground,
the **city limit**.

### 2.5 Blocks, lots, buildings (stages 3–5)

* **Blocks** are assembled by walking each label's chains head-to-tail (with an
  angle rule at the rare junction a face touches twice). Each block records the
  road class of every edge.
* The **buildable core** is the block inset per-edge by
  `half_width + setback + simplification margin`. The offset is a mitred inward
  offset, validated (every vertex and every edge midpoint must be inside with
  the requested clearance, and the result must be simple), falling back to a
  half-plane intersection and then to a binary-searched shrink toward the
  **pole of inaccessibility** — not the centroid, which on a concave cell can sit
  outside the cell entirely.
* **Lots** are the core recursively split along its long axis with a seeded ±13°
  perturbation, the cut offset binary-searched to hit the target area ratio.
* **Buildings** are the lot's oriented bounding box clipped back into the inset
  lot — which reads as a building rather than as a wedge — scaled so footprint
  area tracks `sqrt(file bytes)` as a fraction of the lot, rotated ±4°, and then
  *verified* to lie inside the lot, shrinking until it does.

Because the core is road-clear by construction and everything after it is inside
the core, **buildings outside their lot = 0 and buildings on a road = 0** at the
small scale, and 0 / 14 at 5 k (the 14 come from a documented rescue path for
quarters whose every block refused an offset).

### 2.6 Streets (PRD §9)

Cross-district import edges are aggregated per district pair, and each pair is
routed by **Dijkstra over the junction graph** from one district's gateway
junction to the other's, drawn along the actual road polylines with width ∝
distinct edge count. There is not one chord anywhere on either map.

---

## 3. Measured metrics

Computed by `metrics.rs` from the produced geometry — nothing here is asserted.

### 3.1 `agentolis`, 86 files

```
files 86   districts 26   blocks 52   buildings 86

ROAD GRAPH  nodes(junctions) 82   segments(chains) 136   polyline-segments 6610
ROAD GRAPH  connected components 1    cycles (E-V+C) 55    crossings-without-a-node 1
ROAD GRAPH  degree histogram  2:1  3:57  4:22  5:1  6:1     dangling(deg1) 0
ROAD GRAPH  4-way-or-more junctions 24 (29.3% of nodes)
ROAD GRAPH  classes  arterial 80   perimeter 25   secondary 31

BLOCKS  count 52   no-face 0 (zero-pixel 0)   area min/med/max 1824 / 10245 / 21220
BLOCKS  unbuilt(open ground) 4   no-buildable-core 0
BLOCKS  aspect min/med/max 1.02 / 1.66 / 3.00   slivers(ar>6 or area<12) 0

BUILDINGS  count 86   files-with-no-building 0   outside-their-lot 0   intersecting-a-road 0

DISTRICTS  count 26   sharing-a-border 26   adjacency-components 1   empty-cells 0
DISTRICTS  cell pixels min/med/max 6475 / 63218 / 224975

TIMING  full generation 1038 ms    incremental single-file add 64 us
```

### 3.2 Synthetic 5 122 files

```
files 5122   districts 494   blocks 1319   buildings 5000

ROAD GRAPH  nodes(junctions) 1967   segments(chains) 3294   polyline-segments 33132
ROAD GRAPH  connected components 4    cycles (E-V+C) 1331    crossings-without-a-node 1
ROAD GRAPH  degree histogram  2:4  3:1431  4:406  5:99  6:21  7:6    dangling(deg1) 0
ROAD GRAPH  4-way-or-more junctions 532 (27.0% of nodes)
ROAD GRAPH  classes  arterial 2020   perimeter 137   secondary 1137

BLOCKS  count 1319   no-face 0 (zero-pixel 0)   area min/med/max 19.5 / 357.9 / 3055.3
BLOCKS  unbuilt(open ground) 460   no-buildable-core 388
BLOCKS  aspect min/med/max 1.00 / 2.33 / 14.03   slivers(ar>6 or area<12) 14

BUILDINGS  count 5000   files-with-no-building 122   outside-their-lot 0   intersecting-a-road 14

DISTRICTS  count 494   sharing-a-border 494   adjacency-components 1   empty-cells 0
DISTRICTS  cell pixels min/med/max 850 / 3141 / 27929

TIMING  full generation 1416 ms    incremental single-file add 89 us
NOTE  104 subtrees fell below the legible-quarter size
NOTE  26 quarters needed a rescued block; 114 files could not be housed
```

**Reading the numbers.** Cycles ≫ 0 and zero degree-1 nodes together say the
graph is emphatically not a tree: a tree of 82 nodes would have 81 edges, 0
cycles, and ~30 leaves. This has 136 edges, 55 independent cycles, and no leaves
at all. 4 components at 5 k rather than 1 is three tiny enclave rings (a block
fully enclosed by one neighbour has a boundary loop that touches nothing else);
they are explainable and countable, not shards in a void. The single
"crossing without a node" at each scale is a Chaikin smoothing artefact on one
pair of near-parallel chains — 1 in 33 132 polyline segments.

---

## 4. Determinism

Every draw is seeded from a hash of a logical path through
`polis_layout::determinism` — `fnv1a64_str` + `SplitMix64`, the crate's own
simplex, `det_sin_cos`, `quantize_f64`, `sort_by_f64_key`. No wall clock, no
global RNG, no `DefaultHasher`, no `HashMap` iteration (`BTreeMap`/`BTreeSet`
throughout). Input files are canonically sorted before the tree is built, so an
input permutation cannot reach the layout at all.

The specific hazard the brief flagged — floating-point predicates in a
Voronoi/Delaunay construction — **does not exist here, because there is no
Delaunay construction.** The diagram is evaluated pointwise on an integer
lattice: for each pixel, `argmin` over a handful of siblings of
`|p−s|²/m` with a tie-break on canonical child order. There is no incremental
structure to corrupt, no in-circle test, and cocircular or collinear sites are
not a special case. Sites themselves are quantised lattice positions.

### Proof

Three separate OS processes, the third with the input file list **reversed**
before it is handed to the generator:

```
                                    SHA-256
dump-small.txt   runA/runB/runC   3e1a5684ab9bc6789199e492e6d2145eaea9066e9d1cde6339920a8468283dfa
dump-large.txt   runA/runB/runC   c453ee8bbaac3fe75f9391809d63a3c63d1ddf486246c5defad9846018aa1680
small.png        runA/runB/runC   12445c6f7729b5f5983c022f6d101a92f253ddd64cb392534f7c6bf35d223a32
large.png        runA/runB/runC   658e42c03a98bbd77f3627e828ff7668bf05c700f687dce71925773c13b854ef
junctions.png    runA/runB/runC   afebb9d4fefce7c2cb3b82e6f3be77cc3d0b084e22c3c2e5e6a6c62df4bf938e
junctions-large  runA/runB/runC   09c67c1a782f6f46fefe049e2cedfa83584d121c5a2ba5b66981dd5598778352
```

The dump is a canonical quantised serialisation of every district, chain, block,
lot, building and street. The PNGs are byte-identical too, which is a stronger
statement than the layout dump alone (the encoder is deterministic, so any float
that reached a pixel would show up).

Not proven here: cross-machine and cross-OS identity. Everything in the pipeline
is IEEE-754 `f64` with quantisation at every boundary that reaches output, which
is the right precondition, but only CI on two OSes (PRD §16) settles it.

---

## 5. Incremental growth

Adding one file:

1. Find its district by directory key.
2. Walk to the root checking whether `quantise_count` (buckets at ×1.18) changes
   for the district or any ancestor. If no bucket moves, **no site moves, no
   weight moves, and the partition is bit-identical.** This is the mechanism that
   satisfies PRD §7.7 — the ground does not shift because a file appeared.
3. Append the file to the first block in the district with spare capacity.
   Capacity is derived from block area × the *quantised* count, so it is
   append-only: file *i* lands in the same block no matter how many arrive after
   it.
4. Re-subdivide that one block's lots and re-inset its buildings.

Measured, at 5 122 files: **89 µs**, against a 50 ms budget. The canonical dump
before and after differs in **10 of 15 130 lines** — one block record, its lots,
and the new building. At 86 files it is 64 µs and 4 of 410 lines, and the tool
reports honestly that the district's bucket *did* move (a 7-file directory
cannot absorb a file without changing its bucket) — at that size a resize is
correct and cheap.

When a bucket does move, or a brand-new directory appears, the honest answer is
that the district-level partition has to be recomputed. That is a one-second job
at 5 k files today and it is exactly the case PRD §7.7's ≥800 ms tween exists
for.

---

## 6. What the map makes visible

* **The directory tree.** Quarters nest: `src/`'s subdirectories are inside
  `src/`'s territory, its own files sit at the core. Alphabetically adjacent
  siblings are angularly adjacent. Every district shares a border with at least
  one other (26/26 and 494/494), and district adjacency is one connected
  component at both scales.
* **Git age.** Two independent channels. Radially, older subtrees are placed
  nearer their parent's core, which is nearer the historic centre. Texturally,
  older districts get a ~2.7× finer block grain, so the old town is visibly
  tangled and the periphery visibly coarse. The block fill value also ramps with
  age (darker = older).
* **Size.** Cell area scales with subtree file count via the multiplicative
  weight; footprint area with `sqrt(bytes)`.
* **Wayfinding (PRD §8).** District hue is keyed on the *top-level* tree with
  only a small per-district shift, so quarters of one crate read as one colour
  family — visible in `large.png` as a blue-green north, a green west and a
  maroon south-east. Labels are collision-culled, and a generic leaf (`src`,
  `tests`) is qualified with its parent so the map does not say `SRC` eleven
  times.
* **Streets.** Teal, along the carriageway, thicker where more imports cross.

---

## 7. Where it is weak

1. **Raster resolution is the scaling wall.** Everything is computed on a
   1900² lattice. At 5 122 files the median block is 357 world units² ≈ 34 × 34
   raster cells, which is enough for a clean polygon but not much more. At
   20 000 files the grain would have to be coarser (fewer, larger blocks) or the
   raster larger (memory and time both go as RES²).
2. **388 of 1319 blocks at 5 k have no buildable core**, and 122 files therefore
   have no building. These are blocks narrower than a road is wide. The honest
   fix is to merge sub-road-width cells into a neighbour before face assembly,
   which I did not build.
3. **4 road-graph components at 5 k, not 1.** Three are enclave rings (a block
   fully surrounded by one neighbour). Harmless and explainable but not ideal;
   the fix is to attach an enclave's ring to its host with a short access lane.
4. **The small render reads as a town, not a city, and it should.** 86 files
   over a full frame means each building is ~1 % of the frame. There is no
   layout that makes 86 buildings look like a metropolis; what it *can* do is
   look like a correctly-proportioned village, which it does.
5. **The weight floor caps proportionality at 110:1.** Beyond that a directory's
   area under-represents its size. Deliberate, but it is a lie about scale.
6. **Lots are still visibly the product of recursive bisection.** With 2–4 lots
   per block you get halves and quarters, which reads slightly mechanical up
   close. Real burgage plots run back from the street frontage; cutting
   perpendicular to the nearest street rather than to the long axis would fix it.
7. **No monuments, no industrial massing, no overgrowth, no civic square.**
   PRD §8's landmark layer is stubbed to district labels only.
8. **`assemble_faces` uses an angle rule** for the rare junction a face touches
   twice. It is validated only by the absence of dropped faces (`no-face 0`),
   not by a proof.

---

## 8. Cost to land this in `polis-layout`

**Reusable as-is (~1 900 lines of the prototype's 4 406):**

| prototype file | lands as | note |
|---|---|---|
| `city.rs` §terrain / §pass_a / §pass_b | new `polis-layout/src/partition.rs` | ~700 lines |
| `city.rs` §extract_roads / §assemble_faces | rewrite of `roads.rs` + `blocks.rs` | ~550 lines |
| `city.rs` §lots_and_buildings | `lots.rs` + `buildings.rs` largely intact | ~250 lines |
| `geom.rs` | extends `polis-layout/src/lib.rs`'s `Polygon` | ~650 lines, all pure |
| `metrics.rs` | a golden-file test helper for PRD §16 | ~320 lines |

**Thrown away:** `png.rs`, `raster.rs`, `font.rs`, `render.rs`, `synth.rs`
(~760 lines) — scaffolding so this prototype could produce an image without
touching `polis-render`. The real renderer already exists.

**Deleted from the current implementation:** `roads.rs`'s space colonization,
`plan_districts`, `scatter_attractors`, and the golden-angle placement. `blocks.rs`'s
face-finding is replaced by chain assembly. `terrain.rs` survives unchanged.
`determinism.rs` is **not touched at all** — this design consumed it and found
nothing missing.

**New public types:** `RoadClass::Perimeter`, `Chain` (a road edge is a polyline
between junctions, not a segment), `BlockData::edge_class`, and a `District`
that can represent a collapsed subtree.

**Effort estimate:** two to three days for a competent implementer, most of it in
the crack-graph extraction and the polygon offset, both of which are here and
tested by the metrics. The risky part — proving the partition never yields an
empty or disconnected cell — is the part this prototype spent its time on and
now measures at zero.

**Budget headroom:** 1.4 s cold start at 5 k against a 3 s budget, single-threaded
and unoptimised. The two labelling passes and the component flood fills are
embarrassingly parallel over pixel rows (`rayon` is already a `polis-layout`
dependency, used the deterministic way — `par_iter().map().collect()` into an
index-ordered `Vec`), which should take it well under 500 ms.

---

## 9. Honest critique of my own renders

**`large.png` reads as a city from across the room.** It is one settlement with a
coastline, an arterial skeleton that carries the eye, a fine grain of enclosed
blocks, buildings sitting inside them, and colour families that tell you which
part of the repo you are looking at. It looks like an aerial of a medieval town
that grew, not like a diagram. That is the result I was after and I think it
lands.

**`small.png` does not read as a city, and cannot.** 86 files. It reads as a
village or an administrative map: large quarters, a handful of buildings each,
lots of ground between them. I spent several iterations trying to make it
denser and eventually concluded the arithmetic forbids it — 86 buildings in one
frame are 86 buildings in one frame. What it *does* do correctly is show the
structure the gate is testing: a connected settlement, shared district borders,
closed blocks, lots, buildings, and streets on the carriageway. As a diagnostic
render it is fine. As "a city seen from across the room" it is a hamlet.

**Things I do not like in my own output, specifically:**

* The buildings are pale slabs. They read as *plots*, not as buildings, because
  there is no roof form, no height cue, no silhouette variety (PRD §7.3 asks for
  all three). At the small scale especially they look like spilled cream.
* At 5 k the open-ground blocks (460 of them) are a bit too prominent. Some are
  genuine courtyards; a good third are blocks that failed an offset and got
  quietly demoted, which is a defect wearing a park's clothing. The metric line
  `no-buildable-core 388` is me admitting that.
* The colour is muddy in the middle of `large.png`, where several hue families
  meet at similar values. The age→value ramp and the hue-family scheme fight
  each other there.
* The city limit is a smooth blob. Real towns have ribbon development along the
  arterials leaking into the countryside; here the edge is a clean curve and it
  looks slightly synthetic.
* `docs/` and `docs/verified` in `small.png` end up adjacent and similarly
  coloured, which is correct (they are adjacent in the tree) but makes them hard
  to tell apart.

**The thing I would fix first** is not visual: it is the 388 blocks too narrow
to build on. Merging sub-road-width cells into their neighbour before face
assembly removes the fake parks, houses the last 122 files, and eliminates the
rescue path that produces the only 14 buildings touching a road.
