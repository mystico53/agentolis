# accretion — a Polis city grown one file at a time, in commit order

**Renders**

| file | what it is |
|---|---|
| `small.png` (1600×1600) | the real `agentolis` repository, 86 tracked files |
| `large.png` (1600×1600) | a synthetic 5000-file repository with three years of history |
| `junctions.png` (1600×1600) | the road graph alone, junctions coloured by degree — the "is it a tree?" test |
| `junctions-large.png` | the same at 5000 files |
| `prototype/` | the full prototype source (4 789 lines of Rust, depends on `polis-layout::determinism` and `polis-events::LogicalPath` by path) |
| `small.layout.txt`, `large.layout.txt` | the canonical layout serialisation the determinism proof hashes |

Run: `cargo run --release -- small | large | hash <prefix> | shuffle`.

---

## 1. The idea, and why the previous attempt failed

Space colonisation is a tree-growing algorithm. The PRD says so itself (§7.2): *"Without
snapping you get a tree, and trees read as artificial."* Snapping is supposed to introduce
the cycles — but snapping can only bridge things that are already close, and when districts
are scattered on a golden-angle spiral each attractor cloud is spatially isolated. Snapping
never fires across a gap, so you get one small tree per island. Blocks are the closed faces
of the road graph; no cycles means no faces, which means degenerate lots and degenerate
buildings. That is exactly the failure in `docs/city-m1.png`.

This design does not try to make snapping work harder. It changes what the road network
*is*:

> **The road network is the boundary network of the settled ground.** A road is the line
> where one parcel's territory stops and the next one's begins.

That is a Voronoi diagram of the settled parcels, and it is **planar, connected and full of
closed faces by construction** — not by tuning a radius. Every cell contributes one
independent cycle. There is no parameter setting at which this degenerates into a tree.

The *growth* half is taken at the PRD's word: nothing is partitioned in advance. The town
accretes one parcel at a time in `git log --diff-filter=A --reverse` order, and the districts
are what you get afterwards.

## 2. Pipeline

```
stage 0  accretion    replay commit order; settle plots on the frontier   accrete.rs
stage 1  terrain      fbm simplex from determinism.rs, biases stage 0     accrete.rs
stage 2  roads        Voronoi boundaries of the plots -> weld -> collapse voronoi.rs graph.rs
                      -> prune -> curve -> classify by betweenness
stage 3  blocks       planar face walk of the road graph                  graph.rs
stage 4  lots         recursive subdivision along the longest axis        city.rs
stage 5  buildings    oriented footprints inset inside their lot          city.rs
         districts    majority district per block; borders are road edges city.rs
         streets      cross-district imports, Dijkstra along the roads    city.rs
```

The PRD's non-negotiable ordering (roads → blocks → lots → buildings) is preserved *and
tested*: blocks are recovered by an actual half-edge face traversal of the planar embedding,
not by assuming "one block per parcel". After pruning, 1053 faces come out of 1207 parcels —
they are genuinely different objects.

### Stage 0 — accretion (`accrete.rs`)

State: a list of **plots** (a parcel of ground with a capacity), a running centre of mass per
district, and a spatial hash for min-distance queries.

For each file, in `(growth_index, path)` order:

1. Find its directory's district; create it (and its ancestors) on first use.
2. If a plot of that district has room, the file moves in — **no geometry changes at all.**
3. Otherwise settle one new plot. Candidates are generated on quantised rings around the
   district's centre of mass (plus a ring pre-advanced to its frontier, plus three uniformly
   sampled parcels), and scored on:
   - distance to the nearest settled plot (hug the town),
   - distance to the district's centre of mass (stay compact),
   - terrain slope from `determinism::fbm2_with_gradient` (avoid steep ground),
   - the fraction of nearby plots belonging to *other* districts (don't interleave),
   - adjacency to the district's own ground (a bonus when the nearest plot is a sibling),
   - path-seeded terrain noise (irregularity as residue, not sprinkle).

Two constraints are hard, not weighted:

- **never closer than `sep` to a settled plot** — this is what bounds cell size, and
- **never further than `1.75 × sep` from the nearest settled plot.** A new quarter cannot be
  founded across a gap. This single rule is why `components = 1` at both scales. It is only
  relaxed if no touching position exists anywhere in the plane.

`sep` and the plot capacity both ramp with growth progress: the historic core is settled at
`sep = 1.00` with 3 files per plot, the recent rim at `sep = 1.85` with 7. The old town
therefore has a *finer* street mesh and *smaller* blocks than the periphery — PRD §7.1's age
structure expressed as structure, not as a colour ramp.

### Stage 2 — roads (`voronoi.rs`, `graph.rs`)

Cells are built by **half-plane clipping**, not by a Delaunay triangulation: start from a
fixed frame, clip by the perpendicular bisector with every neighbour inside a query radius,
and widen the radius until it provably covers the cell (`2 × rmax ≤ radius`). No degenerate
predicates, no insertion-order sensitivity.

Boundary cells are bounded by a ring of **phantom plots** laid on a lattice wherever the
ground is empty but within `[1.02, 2.15] × sep` of the settlement. Phantom cells are
discarded; the real/phantom boundary becomes the town's perimeter road. **The lattice is
anchored at the world origin, never at the bounding box** — a bbox-anchored lattice would
shift every boundary phantom whenever the town grew by one plot on the far side.

Three operations then turn a honeycomb into something grown:

- **weld** — shared corners computed independently by two adjacent cells are fused on a
  0.004 lattice after `quantize_f64`.
- **collapse** — every boundary shorter than `1.15 × sep_core` is contracted, shortest first,
  with the merged node placed at the mean of the whole contracted chain. Two three-way
  corners become one four-, five- or six-way junction. **This is PRD §7.2's snap, applied
  where it actually bridges something.** It is what moves the 4-and-5+ share of junctions
  from 8 % to 53 %.
- **prune** — a deterministic minority of *interior* boundaries is deleted, merging the two
  parcels either side into one larger irregular block. The probability ramps from 0.05 at the
  centre to 0.21 at the rim, so the periphery gets bigger, more planned blocks. Perimeter
  edges are never pruned, and no endpoint is allowed below degree 3, so pruning can never
  create a dangling road.

Roads are then given a quadratic arc whose bend follows a low-frequency fbm field, so
neighbouring roads curve *coherently* — curvature reads as terrain rather than as jitter.
Finally `classify_roads` approximates edge betweenness from 26 BFS trees and splits the
network into arterial / street / lane. That hierarchy is the wayfinding skeleton (PRD §8) and
it is also what streets route along.

### Stages 3–5

Blocks are faces of the planar embedding, found by the standard half-edge rule (next = the
predecessor, in counter-clockwise order around the arrival node, of the twin). The unbounded
face is identified as the largest face of the minority winding and dropped.

Lots subdivide the block along its longest axis, **reusing the same axis for the children**
until the strip stops being the long one — that is what turns a block into a row of deep
narrow plots facing the street rather than a quadtree of squares. The split fraction is
jittered ±13 % but clamped so *both* halves stay wider than one buildable strip; guarding
only the parent's extent is what leaves the thin end-pieces that no building can stand on.
Lot grain is a fixed fraction of total block area per file, so a 90-file village and a
5000-file city have the same parcel texture and roughly the same occupancy.

A lot is **viable** if a point inside it clears the block boundary — which *is* the road
centre line — by more than the road half-width. Files are seated on viable lots only,
frontage first (a plot on the block edge is built on before one buried in the middle, so
houses line the streets); unoccupied lots stay vacant and read as yards. When a block has
more files than viable lots the surplus **doubles up on the roomiest plots** rather than
being pushed onto a sliver, and the shared plot's interior is subdivided between them.

Buildings are oriented rectangles aligned to their parcel's long axis (i.e. its street
frontage), area ∝ `sqrt(size_bytes)` clamped into the lot, ±4° of rotation, then shrunk by
0.88 until every corner is provably inside the lot *and* at least a road half-width from the
block boundary. **This is a deliberate deviation from PRD §7.2 step 5.** Taking the inset lot
polygon literally puts triangles and wedges on the map wherever subdivision produced a
triangular plot, and a triangular building does not read as a building; I tried it and the
render is in my notes as the worse of the two.

## 3. Why there are cycles and enclosed blocks

Because they are the same object. Every settled parcel's territory is a bounded convex cell
whose boundary is shared with its neighbours; the union of those boundaries is a planar
subdivision of the settled ground. Euler's formula then hands you `E − V + C` independent
cycles, one per parcel, before a single tuning knob is touched:

| | plots | blocks (faces) | cycles (E−V+C) |
|---|---|---|---|
| agentolis | 65 | 55 | 55 |
| synthetic 5000 | 1207 | 1053 | 1053 |

The cycle count equals the bounded-face count exactly, which is the arithmetic identity you
expect and a useful self-check that the face walk is correct.

The *degree histogram* is not free — a raw Voronoi diagram is almost all degree-3. Collapse
is what produces the 4- and 5-way junctions, and it is measurable: at `l_min = 0.34 × sep`
the 4-and-5+ share of junctions is 7.7 %; at `1.15 × sep` it is 53.3 %.

## 4. Measured metrics

Both runs, defaults compiled in, on this machine (Windows 11, `rustc 1.98.0`, release).

### agentolis, 86 tracked files

```
ROAD GRAPH  nodes=119 segments=173 components=1 cycles(E-V+C)=55 crossings_without_node=0
DEGREE HIST 2:43 (36.1%)  3:48 (40.3%)  4:24 (20.2%)  5+:4 (3.4%)
            junction nodes (deg!=2)=76  4-way=24  5+-way=4
            4-and-5+ share of junctions=36.8%  dangling(deg1)=0
BLOCKS      count=55 (open/plaza=0)  area min/med/max=1.637/5.501/18.650
            aspect min/med/max=1.01/1.33/2.26  slivers=0
BUILDINGS   files=86 lots=229 buildings=86  files_without_building=0
            outside_their_lot=0  intersecting_a_road=0
DISTRICTS   count=25  sharing_a_border=25  fragmented(non-contiguous)=2
            district-adjacency components=1
STREETS     routed=34  not_following_roads=0
TIMING      full_generation=6.0 ms
            incremental single-file add: median 0.343 ms, p95 0.520 ms
            of which the local growth step: median 0.162 ms, p95 0.314 ms
            road nodes whose position changed per add: median 10 of 182
PLOTS       65 settled for 86 files
```

### synthetic repository, 5000 files, 13 top-level packages, depth ≤ 7

```
ROAD GRAPH  nodes=1468 segments=2520 components=1 cycles(E-V+C)=1053 crossings_without_node=0
DEGREE HIST 2:252 (17.2%)  3:568 (38.7%)  4:436 (29.7%)  5+:212 (14.4%)
            junction nodes (deg!=2)=1216  4-way=436  5+-way=212
            4-and-5+ share of junctions=53.3%  dangling(deg1)=0
BLOCKS      count=1053 (open/plaza=4)  area min/med/max=0.711/4.318/38.595
            aspect min/med/max=1.00/1.31/3.06  slivers=0
BUILDINGS   files=5000 lots=9416 buildings=5000  files_without_building=0
            outside_their_lot=16 (0.3%)  intersecting_a_road=79 (1.6%)
            (all 79 used the degenerate-lot fallback)
DISTRICTS   count=276  sharing_a_border=276  fragmented(non-contiguous)=102
            district-adjacency components=1
STREETS     routed=160  not_following_roads=0
TIMING      full_generation=296.7 ms   (PRD 13.1 budget: 3 000 ms)
            incremental single-file add: median 3.70 ms, p95 4.27 ms  (budget: 50 ms)
            of which the local growth step: median 0.001 ms, p95 0.622 ms
            road nodes whose position changed per add: median 0 of 2393
PLOTS       1207 settled for 5000 files
```

Definitions, so the numbers can be argued with:

- **crossings without a node** — proper segment intersections between the *drawn* road
  polylines (curves included), excluding shared endpoints, over a grid broad-phase. Zero at
  both scales: the graph is planar as drawn, not just as intended.
- **sliver** — a block with aspect ratio > 6 or area below 6 % of the median. Zero at both
  scales; worst aspect ratio anywhere is 3.06.
- **intersecting a road** — any building vertex closer than the road half-width to any drawn
  road segment. Instrumented to distinguish the two code paths: all 79 come from the
  fallback, i.e. from lots where no interior point clears the road at all.
- **fragmented district** — a district whose blocks fall into more than one edge-connected
  component of same-district blocks.

## 5. Determinism (PRD §7.4)

Every draw is `SeededRng::for_path(path, purpose)` or `for_seed(combine_seeds(...))` from
`polis_layout::determinism`. Noise is `fbm2_f64` / `fbm2_with_gradient` from the same module.
Every angle goes through `det_sin_cos`; every coordinate through `quantize_f64`. No `HashMap`
anywhere — `BTreeMap` and sorted `Vec` only, and every spatial-hash query sorts its result
before use. `atan2` appears once, to order edges around a node; it produces an ordering, never
a coordinate, and ties fall back to edge id.

The proof: `design-accretion hash <prefix>` writes a canonical serialisation of every road
node, edge and curve, every block ring, every lot, every building footprint, every street
polyline and every plot, coordinates printed at `{:.4}` after quantisation. Two **separate
processes**:

```
$ ./design-accretion hash det/A          $ ./design-accretion hash det/B
small 73c3a6bfbc404744 82655             small 73c3a6bfbc404744 82655
large bd7462a612280254 2205549           large bd7462a612280254 2205549

$ cmp det/A.small.txt det/B.small.txt  -> identical, 82 655 bytes
$ cmp det/A.large.txt det/B.large.txt  -> identical, 2 205 549 bytes
$ sha256sum det/A.small.txt det/B.small.txt
cf46eb684de0f36e8a8243b57b6e9c2d8264a2bebc6fe1e6bcc789a968986263  (both)
$ sha256sum det/A.large.txt det/B.large.txt
72bd17db1572f8228481a1ea25efcff59fc8b29a1d779afd07e68209e7039c86  (both)
```

**Input-ordering independence**: `design-accretion shuffle` reverses the file list and rotates
it by `n/3` before handing it to the pipeline, which sorts on `(growth_index, path)` first:

```
$ ./design-accretion shuffle
shuffled-small 73c3a6bfbc404744 82655       # identical to the sorted run
```

The four PNGs are byte-identical across processes too:

```
5bef99eff135a265... junctions-large.png
e32137214a3b2d7f... junctions.png
925565aaf496ee15... large.png
e55b1967ac6fe921... small.png
```

That last check caught a real leak: the legend originally printed the wall-clock generation
time, which made the image non-reproducible while the layout underneath was perfect. Timings
now go to stdout only.

## 6. Incremental growth (PRD §7.4, §7.7)

Adding one file is literally one more growth step; there is no separate incremental path to
keep in sync with the batch path.

- **Most adds change nothing geometric.** The file moves into a plot that has room: one push
  onto a `Vec`, then the containing block is re-subdivided. 20 of 24 probe adds at 5000 files
  were of this kind, which is why the median "road nodes moved" is 0.
- **When a plot must be settled**, only a local neighbourhood can change. A cell depends only
  on plots closer than twice its own radius, so `voronoi::affected` collects the plots within
  `3.6 × sep` of the new one and `rebuild_subset` recomputes just those; `update_phantoms`
  re-tests the lattice in a `4 × sep` window and returns only the phantoms that appeared or
  vanished. On the small repo, where 15 of 24 adds needed new ground, the median displacement
  is **10 road nodes out of 182**.

The measured cost splits into two parts, reported separately and honestly:

| | local growth step | prototype's full downstream rebuild |
|---|---|---|
| agentolis | 0.162 ms median | 0.343 ms median, 0.52 ms p95 |
| synthetic 5000 | 0.001 ms median, 0.62 ms p95 | 3.70 ms median, 4.27 ms p95 |

The second column is what this prototype actually does: after the local cell update it
re-welds the whole cell array and re-walks every face, which is O(V). At 5000 files that is
3.7 ms — inside PRD §13.1's 50 ms budget by a factor of thirteen, so the prototype never
needed the local graph patch. A production implementation should still patch the node/edge
table locally (see §9); the geometry it would need is already local.

## 7. How git age and the directory tree stay visible

**Git age** is structural, not decorative, and shows up in three independent ways:

1. **Mesh fineness.** `sep` ramps 1.00 → 1.85 over the growth sequence, so the historic core
   has smaller cells and a denser street mesh than the rim.
2. **Block size.** Plot capacity ramps 3 → 7, and prune probability ramps 0.05 → 0.21 outward,
   so the periphery has bigger, more merged, more planned blocks.
3. **Radial position.** The centre of the map is the oldest ground because it was settled
   first; a district founded in the last month has no choice but to bud onto the frontier.

In `large.png` this is directly visible: the fine tangle around the centre against the coarse
blocks in `TOKEN` (bottom right, a late package) and the outer rim.

**The directory tree** determines placement, as PRD §9 requires:

- A file's plot is chosen by its parent directory and nothing else.
- A new district's first plot buds off its **parent's** frontier, so `polis-layout/src` sits
  against `polis-layout`, and siblings fan around the parent.
- The foreign-neighbour penalty and the same-district adjacency bonus keep a quarter's parcels
  touching each other, so districts come out as contiguous blobs that tile the map: **every
  block belongs to exactly one district, and every district shares a border with another**
  (25/25 and 276/276).
- Colour is hue-by-top-level-package, so a package reads as one family whatever its internal
  depth.

**Imports never fight the tree.** They contribute nothing to position. They appear only as
streets: cross-district import counts aggregated per district pair, routed by Dijkstra over
the road graph with arterials discounted 28 %. All 34 / 160 routed streets follow the road
network (`not_following_roads = 0`); there is not one chord across open ground.

## 8. Where it is weak — honestly

1. **102 of 276 districts at 5000 files are non-contiguous** (37 %). Their blocks fall into
   two or more disconnected groups. The compactness and foreign-penalty terms reduce this but
   do not eliminate it: a district that runs out of frontier room places a plot behind a
   neighbour's, and once that happens the border is no longer a single closed curve. On the
   small repo it is 2 of 25, so this is a large-repo problem. It is the metric I would attack
   first, and I think the fix is a periodic *re-seating* pass (swap two plots of different
   districts when it lowers both districts' spread) which is cheap and stays deterministic —
   but it is not implemented and I will not claim it works.

2. **Blocks are not perfectly size-uniform.** Max block area is 8.9× the median at 5000 files.
   These are pruned merges of several large rim cells. They are not slivers (aspect ≤ 3.06)
   but they are conspicuously bigger than their neighbours.

3. **79 buildings of 5000 (1.6 %) sit inside the road corridor.** Every one comes from a lot
   with no road-clear interior point. The lot-viability rule keeps files off those lots until
   a block has more files than viable lots, and then something has to give. Doubling up on
   roomy plots absorbed most of it (from 384 down to 79); the residue needs a proper polygon
   merge of unbuildable lots into their neighbours, which I did not write.

4. **Degree-2 nodes are 17–36 % of the graph.** Most are bends in the perimeter road, where a
   real cell meets two phantoms and only one other real cell. They are geometrically correct
   (a road that bends) but they inflate the node count and dilute the degree histogram. A
   production version should dissolve degree-2 chains into polyline edges and report the
   histogram over junctions only.

5. **`sep_core` and `sep_rim` are absolute world units.** The city's physical size therefore
   scales with file count, which is correct, but it means the age ramp is calibrated against
   the *fraction* of history rather than against real time. A repository whose first year
   produced 5 % of its files will not get much of an old town.

6. **The synthetic repository is my own construction.** It has a plausible shape — 13 packages
   with a 20:1 size spread, depth up to 7, a long tail, a vendored industrial zone, files
   arriving in bursts after their directory is born — but it is not a real 5000-file
   repository and I have not run this on one.

## 9. What it would cost to land in `polis-layout`

**Reused unchanged.** `determinism.rs` in full — `SeededRng`, `fnv1a64`, `combine_seeds`,
`mix64`, `quantize_f64`, `det_sin_cos`, `simplex2`, `fbm2_with_gradient`. This design does not
weaken a single one of its guarantees and adds no new source of randomness. `LogicalPath` and
`layout_seed` likewise. `polis-repo::git::GrowthSequence` is exactly the growth order this
needs, including its incremental `extend_to_head`; my `repo.rs` shells out to `git log` only
because I did not want to pull `rusqlite` into a scratch crate.

**Thrown away.** `roads.rs` almost entirely — `plan_districts`, `scatter_attractors` and the
space-colonisation loop (the golden-angle spiral is the bug). `RoadNetwork`'s
`add_node_snapped` / `snap_target` survive as concepts but the snap is replaced by a
distance-ordered edge contraction, which is a different operation with different tie-breaking.

**New code, roughly.** The prototype is 4 789 lines including its own rasteriser and PNG
writer, which polis-render already covers. The layout half is about 2 500 lines:

| module | lines | note |
|---|---|---|
| `accrete.rs` (new) | ~550 | growth simulation, spatial hash, candidate scoring |
| `voronoi.rs` (new) | ~290 | half-plane cells, phantom lattice, subset rebuild |
| `roads.rs` (rewrite) | ~540 | weld / collapse / prune / curve / face walk / betweenness |
| `blocks.rs` (shrinks) | — | the face walk moves into the road module; blocks become a thin type |
| `lots.rs` (partial) | ~250 | needs the general split-ring, the min-width clamp, frontage ordering |
| `buildings.rs` (partial) | ~150 | oriented-rectangle footprint plus the containment loop |
| `geom.rs` | ~490 | `split_ring`, `erode`, `interior_point_avoiding`, `longest_axis` — most of it belongs in `polis-layout::lib` next to `Polygon` |

**Type changes that ripple.** `Lot` needs `files: Vec<u32>` rather than one optional occupant
(shared plots). `Block` needs an `industrial` flag so the renderer can draw a mass instead of
buildings. `CityLayout` needs `package_edges` alongside `district_edges`. All three are
additive.

**Risks I would flag to the lead.** The face walk is the one piece where a subtle bug is
invisible until the block count is wrong, so it wants a golden test asserting
`faces == E − V + C` on a fixed corpus — that identity held at both scales here and is a
cheap invariant. The `f64`-internal / `f32`-at-the-boundary split needs a decision: I ran the
whole pipeline in `f64` and quantised at every stage boundary, whereas `polis-layout`'s
`Point` is `f32`. Quantising to `f32` between stages is fine, but the half-plane clipping must
stay `f64` or near-cocircular sites produce corners that fail to weld.

## 10. My own honest read of the renders

**Does it read as a city?** From across the room, `large.png` reads as a **city plan** — the
kind of thing you would see in an atlas — rather than as an aerial photograph. The arterial
skeleton is there and legible, the fine lane mesh underneath it is there, the district masses
have distinct shapes and a grey industrial zone sits west of centre exactly where the eye
slides off it. Compared to `docs/city-m1.png` this is a different category of object: that was
shards in a void, this is one connected settlement.

**What is wrong with it, plainly:**

- It still reads a little like a **land-use map or a stained-glass panel** rather than a city.
  The cause is that every block is a differently tinted polygon and the buildings, at 5000
  files in 1600 pixels, are 4–6 pixel flecks. I lowered the ground saturation twice and it
  helped, but colour is still doing more work than the network is. At a real interactive zoom
  this concern mostly evaporates — you would see one district at a time — but I cannot claim
  that from a static overview.
- **No road runs more than a couple of blocks straight.** Real cities have long continuous
  streets and this does not. The arterial *classification* gives you a continuous network but
  each edge is one cell wide, so the through-routes wiggle. This is the honest signature of a
  Voronoi substrate and it is the thing I would most want to fix — probably by growing a small
  number of long "primary" desire lines during accretion and forcing plots to align along
  them.
- **There is one visible tentacle** in `large.png` (top, in `CODEC`) and some ragged fringe.
  Accretion sprawls; killing the outward anchor bias fixed the worst of it (the previous
  version had four long arms and a nearly-detached lobe) but the boundary is still lumpy.
- `small.png` is a **village, not a city, and it should be honest about that.** 86 files is 86
  buildings. It reads as a decent small-town cadastral plan — houses lining the streets,
  gardens behind them, a package boundary you can trace — but nobody will mistake it for a
  metropolis, and the "reads as a city from across the room" test is really only meaningful at
  the 5000-file scale.
- `junctions.png` is the render I am most confident about. It is immediately, unarguably a
  network and not a tree: 55 independent cycles, zero dangling ends, and yellow and magenta
  nodes scattered all through it.
