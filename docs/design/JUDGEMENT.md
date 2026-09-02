# Polis M1 city-layout architecture — judgement

Judged on the rendered images and on independently re-measured geometry, not on the packets' prose.
Every number below labelled **[measured]** was computed by this judge from the designs' own shipped
geometry dumps, not copied from their reports.

**Verdict: recommend `accretion` as the base architecture, with one mechanism grafted from
`treemap-arterials` and one new mechanism built. It is not yet shippable as-is — see
[Required changes](#required-changes-before-accretion-ships). Shipping it unchanged would pass the
structural half of the gate and fail the "reads as a city" half a second time.**

---

## 0. Verification pass (what I could and could not check)

| Artefact | treemap-arterials | voronoi-organic | accretion |
|---|---|---|---|
| PNG sha256 == claimed | ✅ all 6 | ✅ all 4 | ✅ all 4 |
| Geometry dump shipped | ✅ `digest-{small,large}.txt` | ❌ **absent** | ✅ `{small,large}.layout.txt` |
| Dump sha256 == claimed | (fnv digests in metrics.txt consistent) | **unverifiable** | ✅ exact match, both |
| Connectivity re-verified | ✅ 1 component both scales | not possible | ✅ 1 component both scales |
| Planarity re-verified | ✅ **0** true segment crossings | not possible | ✅ **0** crossings at polyline level (10,080 segs) |
| Cycle count re-verified | ✅ 27 / 709 exact | not possible | ✅ 55 / 1053 exact |

`voronoi-organic` names `dump-small.txt` (`3e1a5684…`) and `dump-large.txt` (`c453ee8b…`) with sha256 in
its determinism section, but neither file is in `docs/design/voronoi-organic/`. Only its four PNG hashes
are checkable, and those match. **Half of its determinism evidence is unverifiable from what it delivered.**

The accretion PNGs all being exactly 7,682,253 bytes looked alarming; it is a stored-block PNG writer, and
the four sha256s are distinct. Not a red flag.

---

## 1. Per-image observations

### Baseline — `docs/city-m1.png` (the current failure)

What I see: ~10 disconnected polygonal shards floating in black. Pale grey roads emerge from shards and
trail off into nothing, ending in mid-void. Six or seven heavy teal chords slash corner-to-corner across
the entire frame, passing over empty space. Inside each shard, thin white/tan triangular slivers radiate
from a point — fan arms, not blocks. No closed loops anywhere.

The gate's verdict is correct and understated. This is not a settlement; it is a scatter plot with
tie-lines. Every design in the packet is a different category of object from this.

---

### `treemap-arterials`

**large.png** — One connected mass with a coherent wobbled outline. Tan arterials form a legible armature
across the whole frame. Blocks everywhere, no void. From ten feet away this is unambiguously a *filled,
built-up area* — and that alone beats the baseline decisively. But the texture is wrong: the dominant
motif is **triangles**. Everywhere I look I see triangular faces with pale grey strips inset along their
edges and a large dark void in the middle. The district ground hues are so dark and so uniformly green
that with 97 districts I can pick out maybe five. It reads as **crazed ceramic glaze** or a **Delaunay
triangulation with inset strips** — not as a city.

**Brightened crop (mid-frame, 2.3× exposure)** — Confirms it. Almost the entire ground is one green.
District masses are invisible. The triangulated shatter is now unmistakable. PRD §8's wayfinding skeleton
is not delivered at this scale: you cannot see where a district begins.

**detail-large.png (16% of the city)** — This is the honest view and the author said so. Blocks are large
triangles ringed by a thin frame of grey lot-strips around an enormous empty courtyard. Roads meet at very
acute angles; where they do, the road-width polygon blooms into a large brown wedge (visible centre-left)
that reads as a rendering fault. Nothing here reads as urban ground.

**detail-small.png (42% of the 99-file city)** — **Eight boulevards radiating from a single point.** This
is a hard mechanical artefact: a starburst / asterisk at the civic square. It is the same class of tell as
the old phyllotactic spiral — invisible to every metric, obvious in one glance.

**small.png** — A stained-glass panel. Enormous brown road bands (road widths are absolute, so on a
190-unit city they are proportionally huge), big grey triangular "buildings", the central starburst. Does
not read as a city, a town, or a village. It reads as a Tiffany lampshade. The author concedes this.

**junctions.png / junctions-large.png** — **Settles "is it a tree?" — it is not.** Zero red (dangling)
nodes at either scale. Closed cells everywhere. At 5k the mesh is dense, cellular, with green (deg-4) and
yellow (deg-5+) nodes spread evenly. This is a genuine planar network. At 86 files there are only 27 cells,
and two obvious stars (the centre yellow node with 7 spokes, the upper one with 5).

| Question | Answer |
|---|---|
| Reads as a city? | **No.** Large: crazed glaze. Small: stained glass. |
| One settlement or islands? | **One.** Verified: 1 component, both scales. |
| Tree or network? | **Network.** 709 cycles, 0 dangling — verified independently. |
| Blocks sane? | Mixed — see §2.1. Triangles with spikes. |
| Districts legible? | **No.** They tile correctly but are visually invisible. |
| Grown or mechanical? | **Mechanical.** Named artefacts: *triangulated shatter*, *civic-square starburst*, *long straight boulevard chords*. |

---

### `voronoi-organic`

**large.png** — At full-frame this is the best-looking image in the packet. One connected mass with an
organic coastline, a cream arterial skeleton the eye can follow, hue families that genuinely partition the
map (blue-green north, green west, maroon south-east), and readable district labels. From across the room
it reads as **an aerial of a town**. This is a real achievement and I want to credit it plainly.

**large.png mid-zoom crop (500px square, 2×)** — And this is where it falls apart. The fabric is not urban.
Blocks are **long sinuous ribbons** — worms — winding between wide grey bands. Inside each worm, buildings
are **triangular sawteeth**: zigzag fans of cream triangles. Large regions are entirely empty dark ground.
The named artefact is **contour-map lichen / kelp**. Nothing at this zoom resembles a block, a lot, or a
building. The full-frame view flatters this design severely; at the zoom an operator actually works at, it
is worse than treemap's.

**small.png** — Reads as a **political/administrative map of provinces**, not a settlement. And the
buildings are the problem: at 86 files they are **amoeboid cream blobs with concave, jagged outlines** —
they read as ponds, spilled paint, or lichen patches. This directly contradicts the architecture
description ("buildings are the lot's *oriented bounding box* clipped back into the inset lot"). The clip
is against a raster-derived blobby lot, so OBB ∩ lot *is* the blob. The claim is technically true and
materially misleading. See §2.2.

**junctions.png / junctions-large.png** — Genuine networks, and at 5k the *best* junction render for
"grown" character: 406 orange (deg-4) and 126 red (deg-5+) nodes spread uniformly, not clustered into
stars. Zero dangling. But every single edge meanders continuously — there is not one locally-straight
segment anywhere. The road graph reads as **contour lines / brain coral**.

| Question | Answer |
|---|---|
| Reads as a city? | **Full-frame yes-ish; mid-zoom no; small no.** Net: no. |
| One settlement or islands? | One mass, but **4 road-graph components** at 5k (3 enclave rings — explainable). |
| Tree or network? | **Network**, and the least star-clustered of the three. |
| Blocks sane? | **No** — sinuous ribbons. The aspect metric hides it (§2.2). |
| Districts legible? | **Yes — best of the three, by a distance.** Labels, hue families, nested self-districts. |
| Grown or mechanical? | Organic but **not urban**. Artefacts: *lichen ribbons*, *sawtooth building fans*, *amoeba buildings at small scale*. |

---

### `accretion`

**large.png** — Reads as a **town plan of the kind found in an atlas**. One connected mass, a legible
white/cyan arterial web, coloured district masses on the periphery (CODEC, TOKEN, POOL, THEME), a grey
industrial zone west of centre. Small white rectangles are visible as buildings even at full-frame. It
looks like a settlement.

**large.png mid-zoom crop (500px square, 2×)** — **This is decisively the best fabric in the packet.** A
fine mesh of streets; blocks subdivided into lots; small white rectangular buildings lining street
frontages with gardens behind. It genuinely reads as an aerial of a dense old-town core. This is the only
one of the three whose mid-zoom survives.

Two visible faults at this zoom: (1) **district confetti** — colours change every two or three blocks in
the core, purple/green/brown/teal interleaved, so district masses are unreadable exactly where the city is
densest; this is the 102-of-276 non-contiguity made visible. (2) The cyan import routes are drawn so
heavily they compete with the street network.

**small.png** — Best small render in the packet. A legible **village cadastral plan**: buildings are clean
rectangles inside lots, streets form a proper network, districts are coloured and labelled. Not "a city
from across the room" — 86 files is 86 buildings and no layout fixes that — but it is a *settlement*, which
neither of the others manages at this scale. Faults: a very ragged, lobed, tentacled outline, and large
black void wedges *inside* the settlement footprint (visible between the two purple POLIS-WORLD lobes).

**small.png top-left crop (2×)** — I zoomed here because the maroon lobe looked detached. It is not: it
joins the purple lobe along a road at the bottom edge. Buildings are clean rectangles, correctly inset,
sitting inside their lots, with proper carriageway widths. Confirmed good.

**junctions.png** — The most convincing junction render of the three. An irregular street network of a
small town: 24 yellow (deg-4) and 4 magenta (deg-5+) nodes distributed genuinely across the map, zero
dangling. No stars, no radial hub. This is what "grown" looks like.

**junctions-large.png** — Dense, uniform, 436 deg-4 and 212 deg-5+, zero dangling. And it shows something
the others do not: **the core is visibly finer-grained than the rim** — a dense magenta cluster in the
middle fading to coarse pale-blue lace at the edge. That is PRD §7.1's age gradient, structural and
visible. Quantified in §2.4.

The artefact is **soap-foam honeycomb**: cell size is too uniform, and no edge runs more than one cell
before bending.

| Question | Answer |
|---|---|
| Reads as a city? | **Large: yes (town plan). Small: yes (village plan).** Only design that reads as a settlement at both scales. |
| One settlement or islands? | **One.** Verified: 1 component, both scales. |
| Tree or network? | **Network.** 1053 cycles, 0 dangling, 53% of junctions 4+-way — verified independently. |
| Blocks sane? | **Yes — best in the packet by a wide margin.** See §2.1. |
| Districts legible? | **Periphery yes, core no.** Confetti in the dense centre. |
| Grown or mechanical? | Grown, but **too uniform**. Artefacts: *soap foam*, *no through-streets*, *tentacled fringe*, *confetti districts*. |

---

## 2. Claim-vs-image cross-checks

These are the discrepancies. This is the section that matters most.

### 2.1 treemap-arterials' sliver threshold is set just below its own worst block **[measured]**

Reported: `slivers (aspect > 6 or isoperimetric compactness < 0.16) = 4 (0.6%)` at 5000 files.

Measured, from `digest-large.txt`, all 709 block rings:

| metric | treemap LARGE | accretion LARGE |
|---|---|---|
| compactness 4πA/P² — min | **0.167** | 0.375 |
| compactness — p05 / median | 0.320 / 0.540 | 0.527 / **0.722** |
| blocks with compactness < 0.30 | **27 (3.8%)** | **0 (0.0%)** |
| minimum interior angle — min | **6.9°** | 14.5° |
| min interior angle — p05 | **16.8°** | 42.5° |
| blocks with an interior angle < 20° (a spike) | **60 (8.5%)** | **1 (0.1%)** |

The reported sliver threshold is 0.16. The worst block in the design measures 0.167. **The pass mark sits
0.007 below the worst case.** I make no claim about intent, but the effect is exactly the failure mode the
brief warned about: a metric that reports 0.6% while `detail-large.png` shows spikes on every third block.
8.5% of blocks carry an interior angle under 20°.

Also: I cannot reproduce their OBB aspect at all. They report median 1.88 / p95 3.99 / max 8.11; a
rotating-calipers minimum-area OBB over the same rings gives **median 1.07 / p95 1.53 / max 2.19**. Some
different definition is in play. Their aspect figures are not comparable to the other two designs'.

### 2.2 voronoi-organic's block-aspect metric cannot see its own failure

Reported: `aspect min/med/max 1.00 / 2.33 / 14.03, slivers 14`.

The mid-zoom crop shows blocks that are long **curved** ribbons. An oriented bounding box around a
C-shaped or S-shaped worm is compact — aspect near 2 — while the block itself is a 20:1 serpentine strip
with no interior. OBB aspect is structurally blind to sinuosity, which is precisely this design's failure
mode. Compactness 4πA/P² would have caught it; it was not reported, and no dump was shipped for me to
compute it. **The reported block-shape metrics do not measure the shape defect visible in the image.**

### 2.3 voronoi-organic fails a stated gate property outright

The brief lists as a required property: *"Every file gets a building."*

voronoi-organic's own `metrics.txt`: `BUILDINGS count 5000 files-with-no-building 122` on 5122 files
(2.4%), plus `no-buildable-core 388` of 1319 blocks and `unbuilt(open ground) 460`. A further line in
`metrics.txt` that did not make the packet: `NOTE 26 quarters needed a rescued block; 114 files could not
be housed`.

treemap: 5007/5007. accretion: 5000/5000. This is a hard fail, honestly reported in the metrics but not
framed as a gate failure in the packet.

### 2.4 treemap-arterials does not deliver PRD §7.1 — measured, not visible, not present **[measured]**

The architecture claims age is expressed by split jitter (±0.09 rad new vs ±0.61 rad oldest), dissolve rate
(7% new vs 22% oldest), warp amplitude × district age, and older groups taking the sub-face nearer the
civic square. §7.1 demands the old town be *dense and tangled* and the periphery *more planned*.

Median block area by radial quartile, core → rim:

| design | Q1 (core) | Q2 | Q3 | Q4 (rim) | rim/core |
|---|---|---|---|---|---|
| **accretion** | 2.77 | 4.00 | 5.44 | 5.42 | **1.96×** — monotonic, core is finer |
| **treemap-arterials** | 3.24e10 | 2.24e10 | 2.52e10 | 2.92e10 | **0.90×** — no gradient, non-monotonic |

accretion's old town is genuinely twice as fine-grained as its periphery, and you can see it in
`junctions-large.png`. treemap's age signal produces **no measurable grain difference at all** — it is
decorative, exactly what §7.1 forbids. This is a substantive gap between the packet's prose and its output.

### 2.5 The through-street failure, quantified **[measured]**

I built "strokes" (natural-road continuation, 40° deviation limit) over both shipped road graphs and
normalised by city diameter. This measures whether long continuous streets exist.

| design / scale | longest stroke | strokes > 25% of diameter | median stroke |
|---|---|---|---|
| treemap LARGE | **69%** of diameter | **27** | 4.7% |
| treemap SMALL | 102% | 13 | 23.9% |
| accretion LARGE | **28%** | **1** | 2.7% |
| accretion SMALL | 61% | 9 | 6.3% |

Both self-critiques are correct and now have numbers. treemap has too many city-spanning straight
boulevards (the treemap signature). accretion has essentially **none** — exactly one stroke exceeds a
quarter of the city's width at 5000 files. A real historic core sits between: several strokes at 35–70%
plus a long tail of short ones. This is accretion's single biggest gap and it is a *measurable* target.

### 2.6 The shared root cause nobody named: there is not enough building **[measured]**

Building footprint area as a fraction of block area:

| design / scale | built / block | lots / block | mean buildings per block |
|---|---|---|---|
| treemap SMALL | 10.6% | 34.8% | 3.2 |
| treemap LARGE | **11.7%** | 45.3% | 7.1 |
| accretion SMALL | 7.4% | 66.3% | 1.6 |
| accretion LARGE | **8.9%** | 52.2% | 4.7 |

A dense historic core runs 30–60% ground coverage. Both designs are at 9–12%. **This, more than any
topology property, is why all three read as diagrams rather than cities**: the ground is 90% empty. Both
authors sensed it ("courtyards too big", "buildings are 4–6 pixel flecks") but neither identified it as the
primary cause or measured it. It is a calibration fix, not a rearchitecture — and it applies to whichever
design wins.

Block-size hierarchy is also too flat everywhere: p95/p05 block area is 7.0× (treemap) and 6.8×
(accretion). Real cities span two orders of magnitude between a burgage plot and an industrial parcel.

### 2.7 accretion is the only packet whose independent measurement came out *better* than its self-report

It reported `aspect min/median/max 1.00/1.31/3.06, slivers 0`. I measure median compactness 0.722 (a square
is 0.785), p05 minimum interior angle 42.5°, and exactly one spiky block in 1053. Its sha256 digests match
to the byte, its component count, cycle count and planarity all reproduce exactly, and its planarity holds
at the *polyline* level (10,080 sub-segments, zero crossings) — a stricter test than the node-level one it
claimed. Nothing in its packet was overstated.

---

## 3. Scores

1–10, 10 best.

| Criterion | treemap-arterials | voronoi-organic | accretion |
|---|---|---|---|
| **(a) Reads as a city** | **2** — crazed glaze at 5k, stained glass at 86 | **3** — full-frame good, mid-zoom lichen, small-scale amoebas | **6** — town plan at 5k, village plan at 86; only one that works at both |
| **(b) Structural correctness** | **7** — topology verified perfect; 8.5% spiky blocks, 39 on-road buildings | **4** — 4 components, 1 planarity violation, **122 files with no building**, 388 blocks unbuildable | **8** — verified 1 component, 0 crossings, 1053 cycles, best block geometry measured; 79 on-road buildings (1.6%) |
| **(c) Determinism robustness** | **9** — strongest argument (only exactly-rounded ops, fixed 44-iter bisection, table-driven rasteriser); brittleness is geometric not numeric | **7** — soundest predicate story (no Delaunay, integer-lattice argmin) but **dumps not shipped**, and a raster→DP→Chaikin chain amplifies any flip | **8** — digests verified exact; caught a real wall-clock leak; the f64 half-plane requirement is a named, testable risk |
| **(d) Incremental / §7.7** | **9** — hard structural guarantee: nothing outside a district's parent-fixed polygon can move. 1.3 µs / 0.51 ms | **5** — 89 µs common case, but a bucket change or new directory forces a **~1 s** global repartition | **8** — no separate incremental path at all; median **0 of 2393** nodes moved per add at 5k; 3.70 ms |
| **(e) Git age + directory tree visible** | **6** — tree perfect (it *is* a treemap); age measured at **0.90× — absent** | **7** — best tree expression (labels, self-districts, nesting); age is a value ramp that fights the hue scheme | **5** — age genuinely structural (**1.96×** core-to-rim); tree **broken**: 102/276 districts non-contiguous, confetti core |
| **(f) Scaling to 5k / §13.1** | **10** — 33.6 ms cold, 0.51 ms incremental | **5** — 1416 ms of a 3000 ms budget, single-threaded; RES² raster is a hard wall at 20k | **8** — 297 ms cold, 3.70 ms incremental; O(V) tail flagged |
| **(g) Implementation cost & risk** | **7** — ~700 new/~600 rewritten; two silent-failure invariants (convex rim, 2% area guard) with no analytic bound on warp amplitude | **6** — ~1900 lines land; new raster infra, new Chain type, collapsed-subtree District | **6** — ~2500 lines, largest; additive type changes ripple (`Lot.files: Vec`, `Block.industrial`, `package_edges`) |
| **Weighted overall** | **5.6** | **5.0** | **7.0** |

---

## 4. Recommendation — `accretion`, plus two grafts

`accretion` wins on the criterion that actually failed last time. It is the only design whose image reads
as a settlement at *both* scales and at the mid-zoom an operator will actually work at, and it is the only
one whose every structural claim reproduced under independent measurement — with block geometry that came
out better than advertised. Its road network is a genuine grown network (53% of junctions 4+-way, zero
dangling, verified planar at polyline resolution), and it is the only design that makes PRD §7.1's age
gradient structurally real rather than decorative.

Its two failures are specific, named, and each has a proven donor sitting in this same packet.

### Graft 1 — take treemap-arterials' recursive tree partition as a *territory constraint*, not as a road generator

**The problem it fixes:** accretion's fatal flaw is that district membership is *emergent* — a district is
whatever falls out of where plots happened to land. Hence 102 of 276 districts non-contiguous at 5k, and
the confetti core in the mid-zoom crop. PRD §9 is binding and this violates it.

**The mechanism, precisely:** run treemap-arterials' **Pass 2 only** — the balanced recursive subdivision
of the convex rim, children ordered by `(oldest file, path)`, split at the index best balancing quantised
subtree weight, face cut by area ratio, split normal perpendicular to the longest axis and leaned 30%
toward the terrain gradient. Use its output as a **district territory map and nothing else**. Then accrete
plots as accretion already does, with one added hard constraint: *a plot may only be settled inside its own
district's assigned polygon.* Accretion keeps all five of its frontier scoring terms; they now operate
inside a fixed envelope.

**Why this specific graft:** it delivers, by construction, every §8/§9 property treemap gets right and
accretion gets wrong — contiguous districts, shared borders, legible boundaries, adjacent directories
adjacent on the ground. And it hands accretion treemap's *strongest* asset for free: because a district's
outer polygon is fixed by its parent, re-accreting inside it cannot move one thing outside it. That is the
§7.7 guarantee upgraded from "measured at median 0 nodes moved" to "structurally impossible to violate".

**What is explicitly NOT taken:** treemap's roads. Its chord-splitting *is* the crazed-glaze artefact, its
arterials are the 27 city-spanning straight strokes, and its snap-clustered 7-, 8- and 9-way stars are the
starburst. Roads continue to come from the Voronoi of the accreted plots — that is what produces
accretion's compact blocks (median compactness 0.722), its genuine 4/5-way junctions, and its city-like
fabric. **Partition from treemap; roads from accretion.**

### Graft 2 — desire lines, seeded from the district skeleton (new mechanism, author's own proposal)

**The problem it fixes:** longest stroke = 28% of city diameter, exactly **one** stroke over 25%. No city
looks like that.

**The mechanism:** the district-territory boundaries produced by Graft 1 are already a polyline network —
promote them to *desire lines* before accretion begins. During accretion, any plot whose centre falls
within ~1.5·`sep` of a desire line has its site projected onto a lattice aligned to that line. A run of
co-linearly-seeded sites produces a run of collinear Voronoi boundaries, i.e. an actually straight street.
This is the author's own proposed fix; my stroke measurement says it is the highest-value single change
available.

**Acceptance target:** longest stroke **35–70%** of city diameter, **≥ 8** strokes above 25%, at 5000
files. Both bounds matter — above 70% you have reinvented treemap's boulevard chords.

### What I take from voronoi-organic

Nothing structural. Its partition is worse than treemap's (494 districts, 104 collapsed subtrees, 122
unhoused files), its road substrate is worse than accretion's, and its raster is a scaling wall. But its
**rendering and labelling** is the best in the packet by a distance — hue families keyed on the top-level
tree, district labels placed in-map, a drawn city limit. When polis-render styles the winner, copy that
scheme. That is a presentation decision, not an architecture one, and it should not influence this choice.

---

## Required changes before `accretion` ships

Ordered by how much each moves the "reads as a city" needle.

1. **Raise built coverage from 8.9% to ~30% in the oldest districts.** [§2.6] This is the largest single
   cause of all three designs reading as diagrams, and it is pure calibration of land-per-file. Do this
   first; it is cheap and it changes the image more than anything else on this list.
2. **Graft 1 — constrain accretion inside a treemap-style district territory partition.** Fixes 102/276
   non-contiguous districts and the confetti core. Acceptance: **0** non-contiguous districts at 5k, and
   every district's border traceable as a single curve.
3. **Graft 2 — desire lines for through-streets.** Acceptance: longest stroke 35–70% of diameter, ≥ 8
   strokes above 25%.
4. **Fix the 79 on-road buildings (1.6%) and 16 outside-lot buildings (0.3%).** The residue is entirely
   from the degenerate-lot fallback. Merge unbuildable lots into a neighbour polygon before seating rather
   than falling back. Acceptance: **0** buildings intersecting a road. Do not ship a fallback that puts a
   building in the carriageway.
5. **Widen the block-size hierarchy.** p95/p05 is 6.8×; target ≥ 30×. The uniform soap-foam grain is the
   remaining "not grown" tell after strokes are fixed. Vary `sep` by district age *and* by district file
   count, not by growth-sequence fraction alone.
6. **Tame the fringe.** The tentacle in `large.png` and the void wedges inside `small.png`'s footprint. Add
   a concavity penalty to the frontier score against the settlement's own hull.
7. **Pin the f64 decision and the invariant test.** Half-plane clipping stays f64, quantise to f32 only at
   stage boundaries — the author's own flag, and the failure mode (near-cocircular sites fail to weld →
   silent disconnection) is exactly the disease this whole design exists to cure. Golden test asserting
   `faces == E − V + C` on a fixed corpus; it held exactly at both scales here, so it is a free tripwire.
8. **Calibrate the age ramp against real time, not sequence fraction.** A repo whose first year produced 5%
   of its files currently gets no old town. §7.1 says the first year is the old town.
9. **Run it on a real 5000-file repository.** All three large-scale results rest on the designer's own
   synthetic corpus. accretion's author flags this himself. Before the gate, run it on something real.

Also carry forward regardless of design: **`docs/design/` must be excluded from the repo walk** —
treemap-arterials found that the prototype's own renders were being ingested and the city grew every run.
That trap is now live in this repository.

---

## Notes for the record

- **Nobody should ship treemap-arterials' road generator**, but its `split_face` / `split_edge`
  arrangement primitive is genuinely excellent engineering and its determinism argument (only
  exactly-rounded IEEE ops reach layout; fixed-iteration bisection instead of a convergence loop;
  constant unit-vector table in the rasteriser so the *pixels* hash equal) is the best in the packet.
  If the arrangement is used for the district partition in Graft 1, that machinery comes with it.
- **voronoi-organic's strongest contribution is negative evidence, and it is valuable**: it consumed
  `determinism.rs` in full and found nothing missing. Two independent designs reached the same conclusion.
  The existing determinism machinery is sufficient and should not be touched.
- **The one thing all three got right** is the diagnosis: every design deleted `plan_districts`,
  `scatter_attractors` and the space-colonization loop, and every one of them produced a connected planar
  cyclic graph as a result. The lead's diagnosis is confirmed three times over. Delete the golden-angle
  spiral.

---

*Judged from: `docs/city-m1.png`; all 6 treemap-arterials PNGs; all 4 voronoi-organic PNGs; all 4 accretion
PNGs; 5 additional crops rendered by this judge; and independent geometric re-measurement of
`treemap-arterials/digest-{small,large}.txt` and `accretion/{small,large}.layout.txt`.*
