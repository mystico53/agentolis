# Polis — Visual Review (fresh eyes, no implementer claims read)

> **ROUND 6 is the current review. Round 5 and earlier are archived below the rule.**

---

# Round 6 — the axle was not removed. It was mass-produced.

Reviewer: independent visual pass, given the PRD intent and the §10.3 dimness ruling and nothing
else. Method, in order: 180 px squint sheet before anything else; full frame; 1:1 and 2:1 crops;
then measurement — junction-dot extraction (recovered 116 / 999 / 801 nodes against legend counts
of 116 / 1016 / 802, so the extraction is sound), nearest-neighbour spacing against radius, a
grid search for the best-fitting single centre, boundary box-counting, a luminance census split
into neutral / tinted / text, and a base-vs-overlay difference on the contrast validation.

## Verdict up front: no.

Closer than round 5, and the improvement is real, not a repaint. But it fails, and it fails on
something new that the measurements make unambiguous.

**Round 5 asked whether the axle survived the removal of the wedges. The answer is worse than
"yes". The single axle has become many axles.** `city-m1-large-junctions.png` contains at least
three junction accumulation points; the two largest, at (696,293) and (842,409), are textbook
phyllotactic rosettes — visible spiral arms, spacing collapsing to a point, 19 and 15 nodes
packed at 55–60 % of the map's median spacing. Crop them and you are looking at the sunflower
from round 1 at half scale, twice. `city-real-5k-junctions.png` has the same thing at (1000,883)
with 31 nodes. Distributing the gradient across districts lowers the global correlation number
without removing the artefact — it multiplies it.

And the global gradient is not gone either:

| render | corr(radius, spacing) | median spacing, inner → outer quintile | outer/inner |
|---|---|---|---|
| `city-m1-junctions.png` | **+0.54** | 67.7 → 77.2 → 84.5 → 88.1 → 126.0 | **1.86×** |
| `city-m1-large-junctions.png` | **+0.49** | 28.0 → 28.0 → 28.8 → 31.3 → 38.7 | 1.38× |
| `city-real-5k-junctions.png` | +0.23 | 30.0 → 31.4 → 33.5 → 39.5 → 34.8 | 1.16× |

The small repo is still a clean monotone function of distance from one point — five of five
quintiles increasing, best-fitting axle at (673,846), ρ = +0.55. That is round 5's number
unchanged. The two large renders improved, but their residual is concentrated almost entirely in
the **outermost quintile** — a rind of oversized cells wrapped uniformly around the whole
perimeter. That rind is visible without measuring it: in every junction render the edge of the
map is a chain of large, similar, near-regular polygons at a scale that appears nowhere in the
interior.

## The two textures, and neither of them is grown

Zoom into any district interior in the junction renders and you get exactly one of two things.

**Graph paper.** `city-real-5k-junctions.png` at (330–900, 330–620) — the RUNTIME / SYNTAX
district — is a rectangular lattice of degree-4 junctions. Rows dead straight for fifteen cells,
constant spacing, constant block size. At (820–1180, 760–1010) the same lattice appears rotated
about 30°. Districts get a lattice at their own rotation angle and are sewn together along
irregular seams. Manhattan is a real city, so a grid is not per se wrong — but a *perfect* grid,
with no broken rows, no varying block proportion and no diagonal, is graph paper.

**Sunflower.** The rosettes named above.

A grown city's block size varies with age, terrain and use, and its correlation with distance to
any single point is near zero. Here every district is either a crystal or a spiral. Both are
mathematical objects, and an eye finds them instantly even when it cannot name them.

## The pinwheel — the artefact that appears at every zoom in every image

This is new to this review, and I think it is the most damaging single thing in the pixels.

Brighten any dense crop 3–4× (I used `city-real-django.png` at (700,700)–(1200,1050) and
`city-m1-large.png` at (350,600)–(850,950)) and the fabric resolves into **asterisks**: five to
eight wedge-shaped buildings radiating from a common point inside each block, streets radiating
between them. Almost no building in any of these four maps is a rectangle. They are trapezoids,
triangles, slivers and pie-slices — lot polygons shrunk inward. Blocks are being subdivided by a
fan from an interior seed rather than by frontage onto a street.

This is why five rounds of reviewers have reached for "crazed glaze", "shards", "cracked mud"
and "mould". Those are all descriptions of the same thing: **a Voronoi cell decomposition
rendered directly as architecture.** The dim palette hides it; it does not fix it. And where a
district is sparse, the disguise fails completely — `city-real-5k.png` around AUTOCMD at
(950,880)–(1400,1180) is a bare Lloyd-relaxed Voronoi diagram with white cell walls and four or
five sliver buildings inside it. That is the generator, undressed, printed on the map.

Nothing reads as a city from above without rectangles aligned to a street.

## The coast is die-cut, not ragged

The lobes are genuine and they are an improvement — fingers at the top of django, a bay at the
south-west, the TESTDIR peninsula in neovim. But the roughness exists at exactly one scale.

Boundary box-counting dimension: **0.98, 1.00, 1.00, 1.02** for m1 / m1-large / neovim / django.
A smooth polygon measures 1.00 on this estimator; a real coastline measures 1.15–1.30. Visually:
crop 450 px of django's east coast and it is six straight segments meeting at obtuse angles, with
a uniform pale rim-light stroke and a soft gradient sea outside. No inlets, no estuaries, no
offshore islets, no roughness below about 100 px. It is a sticker.

Circularity is 0.23–0.34, so the shape is genuinely lobed at map scale. It is ragged in the large
and perfectly smooth in the small — the signature of a hull drawn around district centroids, not
of a city that grew into its terrain.

## Height: the primary quantity is not on the map

The legend of every render says it out loud:

- `city-m1.png` — TALLEST `polis-render/src/plan.rs` **H = 50.8**
- `city-m1-large.png` — **H = 6.0**
- `city-real-5k.png` — **H = 6.0**
- `city-real-django.png` — **H = 6.0**

On the three maps that matter, the tallest building in the repository is 6 units, and every
extrusion side-wall in a brightened crop measures the same depth. There is no skyline. There is
no tallest building. **The only way to find it is to read an amber text label.**

Whether that is because the working trees are clean is beside the point for a visual review: the
image cannot distinguish "flat because nothing changed" from "height is not rendered", and
neither can an operator. The map exists to show one quantity and it currently shows it in
typography. On `city-m1.png`, where H = 50.8, height *does* read — thick dark side-walls, an amber
monument with a lit face. So the renderer can do it. The encoding cannot survive a real repo.

## The contrast budget: not wasted downward. Wasted on text.

Being precise here, because round 5's "you cannot see it" is not what I measure.

Within the map, buildings and ground **are** separated:

| | neutral px (building / road) | tinted px (district ground) |
|---|---|---|
| `city-real-django.png` | L p50 = **30.9**, p90 = 40.9 | L p50 = **21.9**, p90 = 23.6 |
| `city-real-5k.png` | L p50 = **32.1**, p90 = 43.9 | L p50 = **16.9**, p90 = 20.4 |

About 11 levels of separation inside a 48-level allowance. That is workable, and the mid-zoom
crops prove it: at 1:1, django and neovim are legible with no boost at all. **Within its dim
palette the map is mostly crisp, not a smudge.** Round 5's diagnosis was wrong, or has been fixed.

Two real problems remain, and both are about *allocation*, not brightness:

1. **The city occupies half its own allowance.** The interquartile range of in-map luminance is
   15.9–32.8 (m1) and 17.5–31.1 (django) — a span of **13–17 levels out of the 48 available**.
   The ceiling at 48 is barely approached except by roof highlights. There is room to widen the
   building/ground separation and put a real tonal ramp on roofs without touching the ceiling.

2. **The labels sit at L = 142 in every single image, without exception.** That is three times
   the base-map ceiling of 48, and it lands *inside the M4 agent band, 97–168*. On `city-m1.png`
   there are 32,116 such pixels. The brightest thing on a map whose thesis is "the coastline is
   always faint; the storm gets the ink" is the word POLIS-RENDER/SRC. The typography is
   squatting in the band reserved for live agents — including on the image built to prove the
   bands are respected.

## `contrast-validation.png` — half of it works

**The point and line marks work, cleanly.** The red route, the red and salmon squares, the cyan
rings, the yellow diamond all separate from the base without effort. The band scheme is sound for
marks, and the dim base does exactly its job underneath them. Good.

**The cloud layer does not work.** Measured against the django base: the overlay lifts **36 % of
the city** by more than 6 levels, and under those clouds the base median goes from L 22 to L 45.
Absolute local detail survives (9×9 σ falls 8.7 → 6.7, 77 % retained), but the *ratio* collapses —
±7 around 45 instead of ±9 around 22 is less than half the perceived contrast. That matches what
the eye reports: the AUTH_TESTS / MODELS / TEXT quarter and the RELEASES / DOCS / REF lobe are
fogged into pale grey mush. You lose the map exactly where the activity is, which is backwards.
Clouds need to be a texture or a hatch the city shows through, not an area fill that raises the
floor.

## Squint test, 180 px

| render | reads as |
|---|---|
| `city-m1.png` | A cracked ceramic tile, or a stained-glass fragment with the caption bigger than the picture. Object, not place. |
| `city-m1-large.png` | Moss on slate. Lobed and textured, with one conspicuous smooth grey wedge (CODEC) that reads as a chip out of it. |
| `city-real-5k.png` | A bruise. Two flat colour zones, navy over plum, with grain. Closest of the four to a place. |
| `city-real-django.png` | Lichen on a rock, cut in half by a ruler-straight diagonal. Object. |
| `contrast-validation.png` | A weather map with fog banks — and the only one where a *place* is implied, because the marks give it scale. |
| `REF-accretion-large.png` | A hydrangea head. Bright and chart-like, but unmistakably one connected settlement. |

None of the four base maps reads as a city at 180 px. Three read as a stain on a surface. What is
missing at this scale is not brightness — it is **structure**: no arterials crossing the map, no
dark voids (parks, water, yards), no density gradient. Coverage is 32–33 % *everywhere*, so the
grain is uniform edge to edge, and uniform grain at thumbnail scale is a texture, not a city.

Two of the four also carry a huge flat colour boundary no city has: django is split by a
near-perfectly straight diagonal running the whole width of the map, tests-plum above,
django-green below, with a bright khaki band along the seam. It reads as a fold in paper.

## Mid-zoom — where an operator works

This is the round's genuine success and it should be said plainly. At 1:1 on django and neovim
there is a street network with real junctions, blocks of varying outline, legible building/ground
separation and visible density change. Crop 500 px out of the dense part of django, show it with
no context, and it passes for an aerial. That was not true two rounds ago.

It falls apart on three things: the pinwheel blocks (above); the total absence of road hierarchy
— V = 2411, E = 4747 in django and every one of those edges is the same width and the same tone,
so there is no arterial, no boulevard, nothing to navigate by; and the district ground colour,
which is saturated forest green at L 22 under 68 % of the city. Boost it and django looks like a
village scattered across a golf course.

`city-m1.png` at mid-zoom is a different and much worse story: no streets at all in a 500 px crop,
six enormous slabs at random angles on a green field, and three label strings occupying perhaps a
third of the frame. With 134 lots there is no fabric to look at, and the render flatters the
layout by showing none of it.

## Worst thing in each image

| image | the single worst thing |
|---|---|
| `city-m1.png` | Text is the dominant graphic element. At mid-zoom the labels cover more area than the buildings, and there is not one street in the crop. |
| `city-m1-large.png` | The `CODEC` polygon: an enormous 45°-hatched empty zone, middle-left. A graphic-design fill pattern, not terrain — and the largest single feature on the map. |
| `city-real-5k.png` | The AUTOCMD quarter is an undressed Voronoi diagram — big convex cells, white walls, five sliver buildings. The generator is showing. |
| `city-real-django.png` | The ruler-straight diagonal colour seam across the entire map, with a bright band along it. |
| `city-m1-junctions.png` | The axle is untouched: five of five radius quintiles increasing, 1.86× inner to outer, ρ = +0.54. |
| `city-m1-large-junctions.png` | Two phyllotactic rosettes at (696,293) and (842,409), spiral arms visible. Round 1's sunflower, twice, at half scale. |
| `city-real-5k-junctions.png` | Fifteen cells of perfect rectangular lattice at (330–900, 330–620). Graph paper. |
| `contrast-validation.png` | The clouds smother 36 % of the city — and the base map's own labels sit at L 142, inside the agent band the image exists to protect. |

## Highest-value change to the IMAGE

**Rebuild the inside of the block. Stop fan-subdividing lots from an interior seed; subdivide each
block into rectangular lots fronting onto its bounding streets, with varied depth and frontage,
and let footprint and height vary within the block.**

One change, and it reaches every image at every zoom:

- Wedges, triangles and slivers become rectangles with a shared grain direction. That single
  property is what makes an aerial read as *built* rather than *cracked*, and it is the thing all
  five previous rounds have been circling with "glaze", "shards" and "mould".
- Blocks acquire an orientation, which breaks up the graph-paper districts without touching the
  road graph — a perfect lattice with varied frontage on it reads as a grid *city*.
- Varied footprint gives massing hierarchy, which is exactly what the 180 px thumbnail lacks.
- It removes the naked-Voronoi failure mode in sparse districts, because a lot with frontage
  still looks like a lot when there is only one building on it.

Then, in order:

2. **Make height survive a real repository.** H = 6.0 on three of four maps means the map's
   primary quantity is invisible. Rescale so the top of the observed range always reaches a
   visibly tall extrusion, add a cast shadow proportional to height, and put a tonal ramp on roofs
   across the L 30–48 headroom that is currently unused. Right now the tallest building is found
   by reading.
3. **Give the road network a hierarchy** — a dozen continuous arterials that cross district
   boundaries, wider and one tone brighter than the rest. It is the only thing that will make the
   180 px thumbnail read as a place, and the abandoned `accretion` prototype in this repo already
   had it (the cyan import routes) before it was dropped.
4. **Get the labels out of the agent band.** L 142 for district text on a base map capped at 48 is
   a straight violation of §10.3, and it is why every squint test so far has reported the caption
   before the city. Drop them to the top of the base band, or draw them only on hover.
5. **Break the coast below 100 px.** Box-dimension 1.00 is a polygon. A little noise on the hull at
   two smaller scales costs nothing and buys the word "coastline".

Do not answer this round with a palette change. The palette is, this time, close to right.


---

---

# ARCHIVE — Round 5

# Round 5 — the pie is gone; the map is now invisible

Reviewer: independent visual pass, told nothing except the PRD intent. Method: squint test
(downsample to 180 px — "across the room") before anything else; then full frame; then 1:1
mid-zoom crops; then measurement — junction-dot extraction, radial spacing law, edge-map
comparison against the previous round's committed PNGs, and a luminance census.

## Verdict up front

**Two answers, and they point in opposite directions.**

1. **The geometry genuinely changed this round. This is not a recolour.** Measured against the
   previous round's committed images: edge-map IoU **0.22–0.25**, **37–45 %** of pixels moved by
   more than 8 levels. Side by side, the convex 12-gon is gone, the pie wedges are gone, the ruled
   radial spokes are gone. The outline is now ragged, lobed, with peninsulas and an isthmus. After
   four rounds of "the artefact was repainted", this one is real work. Say so.

2. **It still does not read as a city at a glance — but not because of the layout.
   Because you cannot see it.** 98.5 % of every map lies below sRGB channel 48. The legend
   admits it: `PRD 10.3 LAYERS 1-2 <= L* 20 (CHANNEL 48)`. §10.3 asks for a coastline on a
   weather chart: *always visible, always faint.* You have delivered faint and deleted visible.
   At 180 px, three of the four renders read as **mould on a dark plate**. Not a pie chart any
   more — a stain.

So: **no, it does not yet look like a city — for a new and much cheaper reason than last time.**

## The new artefact, named

Every previous round had one geometric tell. This round has three, and the first is the pie
chart's ghost.

### 1. THE DRAIN — a density singularity with a radial gradient

Extracted every junction dot from the three junction renders and measured nearest-neighbour
spacing against distance from the densest point:

| render | accumulation point | spacing at centre | spacing at rim | corr(radius, spacing) |
|---|---|---|---|---|
| `city-real-5k-junctions.png` | (810, 665) | 18.4 px | 42.8 px | **+0.584** |
| `city-m1-large-junctions.png` | (1064, 729) | 18.0 px | 38.0 px | **+0.553** |
| `city-m1-junctions.png` | (931, 736) | 54.1 px | 110.8 px | **+0.558** |

A grown city has block size varying by *district* — correlation with distance-to-a-point near
zero. Here block size is a smooth monotone function of radius from one interior point, in all
three renders, at the same strength. At that point the nodes collapse into a visible clot of
overlapping dots. **The wedges were deleted; the centre they converged on was not.** Zoom into
the middle of `city-real-5k-junctions.png` and you can trace curved chains of dots spiralling out
of the clot — the sunflower from round 1, still breathing under two rounds of paint.

This is the highest-priority geometric defect. It is not literally a Vogel spiral any more
(sqrt-k radius-law residual 31 %; a true spiral is under 6 %), so it is not "the same bug" —
but it is the same *family*: one origin, everything graded outward from it.

### 2. THE COG — a sunburst fringe around the coast

Brighten any large render 4× and look at the shore. Outside the built land is a ring of dark
triangular spikes, all pointing outward, evenly spaced around the entire perimeter. Those are the
unbounded Voronoi cells of the coastal sites, rendered as terrain. The island wears a gear.
Visible unboosted too, as a slightly different black. Also: the coast is **facetted at exactly
one scale** — every shoreline segment is a straight chord of roughly 30–60 px with obtuse
corners. Real coastlines have detail at every scale; this has detail at one.

### 3. THE ROSETTE — ROOT parked at the geometric centre

In all three large renders, `ROOT`/`DOCS` is a pale grey mass sitting at the middle of the
landmass with the top-level directories wrapped concentrically around it. Django reduces to
green-left / magenta-right / grey-core meeting at the centre — the same tripartite structure as
last round, just with the wedge boundaries smoothed off. Repositories do not have a middle.
Cities have a historic core *and a direction they grew in*; this has an origin.

## Squint test at 180 px, unboosted (the PRD §1 test)

| Render | What the thumbnail is |
|---|---|
| `city-m1.png` | A pile of cardboard boxes on coloured paper, buried under text. **Object.** |
| `city-m1-large.png` | A dark lobed landmass with a lake. **The one that passes.** Place, barely. |
| `city-real-5k.png` | A lichen patch / a butterfly-shaped stain. Place-ish, unreadable. |
| `city-real-django.png` | **A two-tone pill.** Green half, magenta half, near-circular. Object. |
| `city-m1-junctions.png` | A rhombic lattice of dots inside a ring of big empty cells. |
| `city-m1-large-junctions.png` | A dot mesh with a bright clot off-centre. |
| `city-real-5k-junctions.png` | **A sunflower head.** Unmistakable. |
| `design/accretion/large.png` (ref) | A lobed island with coloured districts and peninsulas. **The most place-like image in the repo — and it is the one you rejected.** |

Read that last row again. The archived round-4 reference still beats every current render on the
squint test, because it is bright enough to have a silhouette. This round has the better geometry
and the worse image.

## Can you see building height? No.

- Legend swatches `BUILDING LOW` and `BUILDING TALL` are both dark olive-grey and are, at a
  glance, **the same colour**.
- Buildings now do have extrusion — a pale roof plus a darker side face. That is new and it is
  right. But the tonal range is so compressed that you cannot rank two neighbours, let alone find
  the tallest in a 5 000-file repo.
- The tallest is disclosed **only as text in the legend** — `TALLEST CORE/MEDIA/INDEX/METR…` —
  and it is *still clipped by the right edge of the canvas*, exactly as flagged last round.
- In `city-m1.png`, height is drawn as **nested concentric rectangles**: the tall building is a
  bullseye. It reads as a target or a picture frame, not a tower, and comparing two heights means
  counting rings.

Height is the primary quantity the map exists to show. It is the least visible thing in the image.

## Mid-zoom: this is where the round succeeds

At 1:1 the fabric is genuinely good and deserves saying so. The off-centre crop of
`city-m1-large.png` — TOOLS / MOBILE / PACKAGES — looks like a night aerial of a dense old
European town: irregular blocks, varied footprints set along street frontages, streets that bend
and T, a black void that reads as a park. `city-real-django.png` holds up the same way. No tiling,
no repeat, no visible grain.

Two mid-zoom faults: the **centre crops are visibly finer and mushier** than the off-centre ones
(the drain again), and several district cells are **completely bald** — a flat coloured polygon
with no buildings at all (`CMAKE` in 5k, several on the django rim).

## Is the base map faint enough for a bright overlay?

Overwhelmingly yes, and that is the problem. Land pixels above channel 48: **1.1–1.6 %**. Above
channel 120: **0.5–0.7 %** — and all of that is label text, not city. An agent trail would stand
out against this like a flare; so would a single lit pixel. §10.3 wants a coastline you can always
see. You have a coastline you can never see.

## The single worst thing in each image

| Image | Worst thing |
|---|---|
| `city-m1.png` | Buildings the size of city blocks, and labels wider than the districts they name. A label cloud with a map behind it. |
| `city-m1-large.png` | It is the best geometry in the set and it is invisible. Contrast p95−p5 = **28** out of 255. |
| `city-real-5k.png` | The centre is a mush of the finest blocks on the map, sitting exactly where the eye lands first. |
| `city-real-django.png` | Bisected disc — green half, magenta half, clean seam through a grey core. Reads as a pill, not a place. |
| `city-m1-junctions.png` | A lattice core inside a ring of huge empty polygons: two unrelated fabrics with a hard join. |
| `city-m1-large-junctions.png` | The bright accumulation clot right of centre, with dot spacing graded smoothly outward from it. |
| `city-real-5k-junctions.png` | **A sunflower head.** This image is the diagnosis; do not ship geometry that produces it. |

## The highest-value change to the IMAGE

**Raise the base map's ceiling from L\* 20 to roughly L\* 45–55, and spend every bit of the new
range on building height.**

Concretely: leave terrain, district fill and roads near their current tone — they are already
right, and they are what §10.3 is protecting. Then give roofs a full ramp from near-background to
near-white driven by uncommitted diff lines, and give each building a cast shadow whose *length*
grows with height. Two encodings of the same quantity, both legible at 180 px. The tallest
building should be findable across the room without reading the legend.

That one change fixes the squint test and the missing primary quantity together, and it does not
touch the layout — which, for the first time in five rounds, mostly does not need touching.

**Second, and only second: kill the drain.** Block size must be a property of the district, not a
function of distance to an origin. Target corr(radius, junction spacing) below ±0.15. Until that
number moves, every render will keep having a middle.

**Third, cheap:** clip the shore against the land mask so the outward Voronoi spikes stop drawing
a gear around the island, and stop clipping the `TALLEST …` caption at the canvas edge — that has
now survived two reviews.

**Do not answer this review by darkening anything.**

---
---

# ARCHIVE — Round 4

## Polis — Visual Review, round 4 (as written at the time)

Reviewer: independent visual pass. Method: full-frame look, squint test (downsample to ~180px
wide and view — this is "across the room"), then 1:1 mid-zoom crops at the scale an operator
actually works at. Plus a pixel-colour census to check what the render is actually made of.

**Verdict up front: NO. None of these read as a city.** Two of them read as a pie chart, one
reads as stained glass, one reads as mould on a petri dish. The failure is obvious in one look,
which is exactly the failure mode this project keeps repeating.

---

## The squint test (the PRD §1 test)

Downsampled to ~180px and viewed as a thumbnail row, here is what the four renders read as:

| Render | What it reads as at a glance |
|---|---|
| `city-m1.png` | A stained-glass coaster / Trivial Pursuit wheel |
| `city-m1-large.png` | **A pie chart.** Unambiguously. Coloured wedges radiating from a centre dot |
| `city-real-5k.png` | An agate slice, or a pie chart with a bite taken out |
| `city-real-django.png` | A two-tone poker chip, or mould on a green agar plate |

Not one of them reads as a settlement. Every one reads as *an object* — a disc — because
the silhouette is a convex ~12-gon floating in hard black, and because saturated categorical
colour, not built mass, is carrying the whole image.

---

## Pixel census (what the images are literally made of)

Sampled over the map area (legend strip excluded):

| | `city-m1` | `city-real-5k` | `city-real-django` |
|---|---|---|---|
| Pure black void `(12,13,17)` | **30.5%** | **30.3%** | **30.0%** |
| Single beige `(197,193,182)` | 9.5% | **17.5%** | 11.0% |
| Everything else | saturated district fills | saturated district fills | two green/purple fills |

Two facts fall straight out of this:

1. **A third of every canvas is empty black.** The city is a coin on a table, not a place.
2. **Every building in `city-real-5k` is the exact same RGB value.** 3,890 buildings,
   one colour, no second tone, no shadow, no outline gradient. Same in django (7,011 buildings).
   `city-m1` has exactly two building tones.

That second fact is the whole problem in one number. **Building height — the PRD's primary
encoded quantity — is not visible anywhere in any image.** There is no extrusion, no drop
shadow, no tonal ramp, nothing. Flat-fill rectangles at a single value. If you rendered the
diff-lines column as literal noise you would get the identical picture.

---

## Per image

### 1. `city-m1.png` — real Polis repo, ~90 files

**Literally:** A near-regular 14-sided polygon filled with ~100 convex polygon cells, each
flooded with a strongly saturated flat colour — plum, maroon, navy, teal, forest, olive, brick.
Inside each cell float one to three rounded grey or cream rectangles with generous margins
around them. Occasional saturated yellow rectangles. Cell edges are drawn as pale cream/white
strips. Black surround.

**Reads as a city?** **No.** Not at a glance, not at full attention. It reads as stained glass,
a Trivial Pursuit board, or a colour-swatch fan. The dominant visual is the *colour*, and colour
here is a categorical directory key, so the eye correctly parses it as a choropleth legend, not
terrain.

**Artefact:** Voronoi/Lloyd-relaxed **foam**. Near-equal-area convex cells, almost every junction
a 120° Y. This is soap bubbles, not streets. The cream cell-borders-as-roads means *everything*
is a road, which means nothing is: there is no road hierarchy, and not one road runs straight
for more than one cell.

**One settlement or several?** One, and connected — the junction render confirms V=124, E=219,
one component, no dangling. Structurally fine, visually inert: every edge is the same length,
same width, same colour. A road network in a real city has segments spanning orders of magnitude
in length and a visible arterial skeleton. There is not one traceable through-route here.

**Districts / landmarks:** Districts are trivially separable — they're painted different colours,
which is cheating rather than wayfinding. Landmarks: the yellow "monuments" are the *same shape
and size* as ordinary buildings and are scattered without any relation to junctions, squares, or
the centre. Nothing to navigate by. The ROOT district is a dull maroon patch at the bottom edge,
not a centre.

**Mid-zoom:** Dissolves completely. A 600px crop is six saturated polygons with one grey pill
floating in each. At the scale an operator works at there is no fabric at all — no blocks, no
frontage, no street wall. The 90-file case is the one an operator will look at most (it's their
own repo) and it is the weakest image of the set.

**Worst thing:** Density. Cover is ~29% and it looks like 10% — buildings float in the middle of
huge coloured fields with no relationship to the streets that bound them. It is a colour-blocked
diagram with a few pebbles dropped on it.

---

### 2. `city-m1-large.png` — synthetic 5,000-file repo

**Literally:** A convex 14-gon divided into ~8 large wedges of saturated colour — grey, violet,
olive, brown, green, magenta, blue — whose straight boundaries all converge on a single point
just below-left of centre, where a tiny dark-red sliver sits. Each wedge is filled with a foam of
white lines and a uniform confetti of small pale beige rectangles. One wedge (grey, "CODEC") is
completely empty of buildings.

**Reads as a city?** **No — it reads as a pie chart.** This is the single most damning image in
the set. At thumbnail size it is a colour wheel. The radial-fan / starburst artefact that the PRD
explicitly warns about is present in its most literal possible form: straight lines from a centre
point to the rim, sectors of flat colour between them.

**Artefact:** **Radial fan / pie chart** at the macro scale, **Voronoi foam** at the micro scale.
Both are on the forbidden list. The grey CODEC wedge additionally reads as a hole punched in the
disc.

**One settlement or several?** One blob, connected. But the junction render shows a **singularity**
— node density collapses inward to a point at the convergence — and at least one dead-straight
edge running from that centre to the right rim with *no junctions on it at all*. Roads don't
dangle; they do something worse, which is run kilometres without meeting anything.

**Districts / landmarks:** Districts are the pie slices. You can tell them apart, but only the way
you can tell apart the slices of a pie chart. Zero landmarks — I could not find a single yellow
monument pixel anywhere in this render despite MONUMENT being in the legend.

**Mid-zoom:** A 500px crop across the CHART/ADAPTERS/INVOICE boundary shows **two dead-straight
parallel lines running the full diagonal of the frame**, separating magenta from green from teal.
Nothing in any city on Earth has two perfectly straight parallel boundaries running that far. The
mathematical construction is more visible at mid-zoom than at full frame. Between them, uniform
beige confetti at a constant density with no core, no edge, no variation.

**Worst thing:** It is a pie chart. Everything else is secondary.

---

### 3. `city-real-5k.png` — Neovim, ~5,000 files

**Literally:** A convex 12-gon. Upper half magenta/plum, lower half indigo, with a handful of small
saturated patches (olive, teal, brown, green, purple) crammed into the right-centre edge. Two long
straight-sided ribbons — an olive one labelled CMAKE and a purple one labelled SCRIPTS — shoot from
just left of centre out to the right rim and are almost entirely **empty ground**. Everything else
is uniform pale beige confetti threaded with white lines.

**Reads as a city?** **Partly, and only if you already want it to.** The dense lower-left quadrant
(SYNTAX/FTPLUGIN) genuinely has some medieval-town character at mid-zoom. Everything around it does
not, and at a glance the whole thing is a coloured disc with a pale radial smear.

**Artefact:** **Radial fan again**, and this time the empty wedges make it undeniable — the
junction render of this repo looks like **Pac-Man**: two blank triangular bites out of a dot-filled
disc, with the node density funnelling to a point in the middle. Plus the same Voronoi foam
throughout, plus one detached-looking node cluster hanging in the empty top-right area.

**One settlement or several?** Nominally one connected component, but visually it is one dense town
plus several disconnected-feeling scraps stuck on the right edge (RELEASES, WORKFLOWS, REF, HOWTO)
separated by empty wedges — the "disconnected shards" impression the project already failed on once.

**Districts / landmarks:** Districts are legible only by colour. Landmarks: I can find two or three
tiny orange specks in 5 megapixels. The **ROOT district — the most important place in the repo — is
a ~40px dark-red scrap** sitting at the pie-chart singularity. The centre of the city is a crumb.

**Mid-zoom:** Mixed and revealing. Crop A (COMPILER/DOC/FTPLUGIN) is the best fabric in the whole
review — irregular blocks, buildings roughly fronting streets. Crop B (TESTDIR/EX_CMDS) is the
worst — beige flakes scattered on a hot magenta field with half the frame being empty polygons,
reading as cornflakes or peeling paint. So the fabric holds in maybe 30% of the map and dissolves
in the rest. And in *both* crops the roads are one uniform white width — no hierarchy at all — and
the ground colour is so saturated that unbuilt space reads as water rather than land.

**Worst thing:** Two enormous, dead-straight, completely empty wedges radiating from the centre.
They are the mathematical construction showing through the skin, and they are visible from across
the room.

---

### 4. `city-real-django.png` — Django, ~7,000 buildings

**Literally:** A convex 12-gon, roughly 60% flat mid-green (lower-left), 30% flat plum (upper-right),
plus a cluster of small coloured patches on the right edge. Uniformly scattered over the whole thing:
identical pale grey-beige rectangles, all about the same size, at essentially constant density, with
thin white lines cracking between them.

**Reads as a city?** **No.** It reads as lichen, or mould on a green agar plate, or a rash.

**Artefact:** No single named artefact dominates at full frame — this is the least *geometrically*
tell-tale of the four. But the texture artefact is total: **uniform confetti**. The white lines
branch dendritically like leaf venation or mycelium rather than forming blocks.

**One settlement or several?** One, connected, dense, and homogeneous to a fault.

**Districts / landmarks:** Two giant colour fields and a scatter of tiny ones. You cannot navigate.
There is no centre — the ROOT district is a small dark-red patch shoved against the right edge with
the small change. Cover is 21%, so ground dominates.

**Mid-zoom:** **Worst mid-zoom in the set.** A 700px crop is 100% homogeneous: same-size grey
rectangles, randomly rotated, evenly spaced, on flat green, with white cracks. There is no core, no
edge, no landmark, no scale variation, no alignment to any street. It is a *texture swatch*. You
could tile it infinitely. If mid-zoom is where the product lives, this render has no product in it.

**Worst thing:** Total homogeneity. Every building is the same size, the same single colour, the
same random rotation, at the same density, everywhere. That is what makes it read as gravel on
grass instead of a city.

---

## The junction renders

`city-m1-junctions.png` and the two large ones are honest and show the network is connected, cyclic,
and free of dangling ends. That is real and worth keeping. But they also expose the geometry:

- **Uniform edge length.** Every segment is roughly the same length. Real road networks are wildly
  heterogeneous and have a long-distance skeleton.
- **No hierarchy.** All edges drawn at one weight because they *are* one class.
- **A singularity at the centre** in both 5k renders — node density funnels to a point.
- **Kilometre-long edges with zero junctions** on them (the wedge boundaries), which is the exact
  opposite of the "real junctions" the graph metrics claim.

Passing `DANGLING=0 COMP=1 CROSSINGS=0` is necessary and nowhere near sufficient. All four earlier
failures would have passed it too.

---

## Against the three earlier prototypes

- `design/treemap-arterials/large.png` — triangulated crazed glaze, starbursts everywhere. Dead.
- `design/voronoi-organic/large.png` — contour lines / topographic map / lichen ribbons. Dead.
- `design/accretion/large.png` — **this one has the single thing M1 threw away: a ragged, lobed,
  non-convex coastline with peninsulas and inlets.** It reads as a place occupying terrain. Its
  failure is the opposite of M1's: per-cell hue randomisation makes it camouflage. M1 fixed the
  colour noise and then regressed the silhouette to a coin. The right answer takes accretion's
  outline and neither one's palette.

---

## Overall verdict

**No. This does not look like a city.** It looks like a pie chart at 5,000 files, stained glass at
90 files, and mould at 7,000 files. The one thing that half-works is the mid-zoom fabric in the
densest quadrant of the Neovim render, and it works in maybe a third of that image.

### Highest-value change to the image

**Rebuild the building layer.** It is currently 3,890–7,011 rectangles at *one* RGB value, one size
class, one random rotation distribution, and one constant density, floating in the middle of blocks
with margins on all sides. That single fact is why every crop reads as gravel, confetti, cornflakes,
or lichen, and it is why the PRD's core encoding (height = uncommitted diff lines) is invisible.
Specifically:

1. **Give buildings a tonal range and a drop shadow.** Height must be *seen*, not stated. A dark
   offset shadow proportional to height, plus a roof-tone ramp, instantly converts flat flakes into
   massing — and it is the cheapest change in the list.
2. **Widen the footprint distribution to ~10x** within a block. Uniform building size is the loudest
   "generated" signal in the image after the pie wedges.
3. **Push buildings to the street frontage and let them share party walls.** Buildings should form a
   contiguous built *mass* with the block edge and leave the courtyard open, not float as isolated
   islands in a coloured field. This is what makes an aerial photograph read as a city.
4. **Add a density gradient** — dense core, thinning to the edge. Constant density everywhere is
   what makes django tileable.

Two things must die alongside it or the fix will not survive the squint test:

- **The radial wedge partition.** Straight boundaries converging on ROOT are a pie chart and no
  amount of texture will hide them. This is the mathematical tell.
- **The saturated categorical district palette.** Drop the ground to a narrow, low-chroma, dark
  range and let built mass carry the image. Right now colour is doing 100% of the work at a glance
  and it is spelling "chart".

And while you are in the renderer: the convex 12-gon silhouette and the 30% black surround make the
city read as an object rather than a place. Take the ragged coastline back from `accretion`.

---
---

# ROUND 2 — second independent fresh-eyes pass (2026-09-02)

Reviewer: a second independent pass, told nothing about what was claimed or changed. Same method:
squint test at 180px FIRST, then full frame, then 1:1 mid-zoom crops, then a pixel census.
Images reviewed: `docs/city-m1.png`, `city-m1-large.png`, `city-real-5k.png`, `city-real-django.png`,
and the three junction renders, against the prototypes in `docs/design/*/`.

## Verdict: still NO. And it is the SAME pie chart as Round 1.

I wrote my findings before reading the Round 1 section above. They match it almost line for line.
Then I measured why.

**Structural edge-difference between `render-review/before-large.png` and `after-large.png`: 5.4% RMSE.
Between `after-large.png` and today's `city-m1-large.png`: 2.3% RMSE.** The geometry has not moved.

Round 1 gave four recommendations. Exactly one was implemented:

| Round 1 said | Status |
|---|---|
| Drop the saturated categorical palette; make the ground dark and low-chroma | **DONE.** Genuinely done, and it worked — see "credit where due" |
| Kill the radial wedge partition converging on ROOT | **NOT DONE.** Untouched |
| Take the ragged coastline back from `accretion`; lose the convex 12-gon | **NOT DONE.** Untouched |
| Give buildings a tonal range and a shadow so height is *seen* | **NOT DONE.** Measurably not done |

The pie chart was **recoloured, not removed**. Turning the saturation down made the wedges harder to
see in a screenshot and did nothing to the thing that makes them wedges. This is the third cycle in
which a geometric failure has been answered with a tonal fix.

## 1. Squint test (180px)

| Render | What the thumbnail reads as |
|---|---|
| `city-m1.png` | A dark stained-glass coaster. Lumpy disc, grey slabs on tinted glass |
| `city-m1-large.png` | **Pac-Man.** A dark coin with an empty pie slice cut out of the top-right, apex on the centre |
| `city-real-5k.png` | A polished agate / geode slice. Coin outline, a wedge at 3 o'clock |
| `city-real-django.png` | **A two-colour pie chart with a fan of thin slices on the right.** Textbook |
| `city-m1-junctions.png` | A wire mesh disc — acceptable, this one reads as a graph and is meant to |
| `city-m1-large-junctions.png` | A dartboard. Radial spokes and a ruled diameter across the middle |
| `city-real-5k-junctions.png` | **A sunburst diagram.** Straight spokes to a singularity at dead centre |

Every single one is an **object with a rim**, not a place. Not one has an edge that a coastline,
a river, a ridge or a road ever made.

## 2. The mathematical tell, named

**Polar coordinates.** The layout is a sunburst/pie chart in disguise:

- **A singularity at the centre.** In `city-real-5k-junctions.png` the junctions visibly collapse
  into a knot at one point. In `city-real-django.png` the node labelled `ROOT` is the apex that every
  district boundary radiates from.
- **District boundaries are exact radii.** Perfectly straight lines, 700+ px long, from ROOT to the
  rim. `city-m1-large-junctions.png` has a ruled horizontal **diameter** running edge to edge through
  the centre. Real settlements do not contain a straight line that long, ever.
- **The outer boundary is a convex ~12-gon** — a coin with chamfered edges. Real city footprints are
  ragged, lobed and deeply non-convex. This is the single strongest "not a place" signal in the set.
- **Empty wedges.** In `city-real-django.png`, `SCRIPTS`, `.TX`, `EXTRAS`, `RELEASES`, `POSTGRES` are
  flat coloured triangles with **no buildings in them at all**, converging on the labelled centre.
  Empty coloured triangles meeting at a labelled point is not "like" a pie chart. It is one.
- **Sliver districts.** `CMAKE` in `city-real-5k.png` is a 900 px long, 40 px wide spike. A district
  shaped like a spoke is not a neighbourhood.
- **A radial density gradient** — a pale circular bald spot around the centre of `city-real-django.png`
  where lots shrink toward the origin. It reads as a thumbprint, and it is another polar artefact.

`city-m1.png` (115 roads) escapes the worst of this only because at 26 districts there is not enough
of it to see the fan. It is the small-N alibi, not a counterexample.

## 3. Building height is invisible. This is measurable.

Height is the primary encoded quantity and it is not in the picture.

Colour census over the map area of `city-m1-large.png`, every colour brighter than L≈38:

```
103234 px  #2C2C29      <- one roof grey, dominant by 6x
 17892 px  #2D2C29
 10867 px  #282724
  7296 px  #2C2B29
  ... 1183 distinct tones, ALL inside #272725 .. #2F2E2B
```

**Every roof in the city occupies about 8 levels out of 255 — 3% of the available dynamic range.**
There is one roof grey. There is no roof ramp, no shadow, no massing. The extruded side wall is
2–4 px on every building regardless of height.

The proof: `city-real-5k.png` captions its own tallest building as `SRC/NVIM/MAIN.C H=6.0`. I cropped
that location and viewed it at 2x. **I cannot pick it out from its neighbours.** The only thing
marking the tallest structure in the repository is a gold *text label*. The map is carrying its
primary quantity in typography.

Worse: the brightest ink on the base map, `#8A8F98`, is the **label text**. Nothing built is
brighter than the words. The city is a caption with a texture behind it.

`city-m1.png` is the exception — at 115 buildings you can see extrusion, side walls and gold
monuments, and it is the only render in the set where the encoding is legible. That legibility does
not survive contact with a real repository, which is the only case that matters.

## 4. Credit where due — the two things that are now right

- **The base map is faint enough.** Mean luminance over the map area is 20–24/255, max 143. There is
  a clean 110+ level of headroom for a bright "what is happening now" overlay. PRD §10.3 is
  satisfied. Do not undo this.
- **The road network is topologically sound.** `COMP=1`, `DANGLING=0`, `CROSSINGS WITHOUT A NODE=0`,
  deg4+ share 45–57%. It is one settlement and the junctions are real junctions. The problem with the
  roads is not their topology, it is their *geometry* — a polar grid, not a grown network.

## 5. Mid-distance

Mixed, and worth separating:

- The **interior fabric of a large district** holds up. The NW corner of `city-real-django.png` at 1:1
  genuinely resembles an aerial photograph of a dense low-rise town — irregular blocks, capillary
  lanes, no visible tiling. This part is good work.
- The **centre dissolves immediately**. The 700 px crop around ROOT in django is nothing but ruled
  radial lines and empty triangles. The one place an operator will look first is the one place that
  is unambiguously a chart.
- Everywhere, the fabric is **uniform grain**. Same building size, same tone, edge to edge. It reads
  as crushed gravel or cracked mud, because there is no massing hierarchy to read.

## 6. Worst thing in each image

| Image | Worst thing |
|---|---|
| `city-m1.png` | Buildings are the size of city blocks; 26 districts is a village, and it flatters the layout |
| `city-m1-large.png` | The empty `CODEC`/`TESTS` pie slice with its apex on ROOT. Pac-Man |
| `city-real-5k.png` | `CMAKE` and `SCRIPTS` as ruled spokes, plus a visible junction singularity at centre |
| `city-real-django.png` | A fan of ten empty coloured triangles converging on a point labelled ROOT |
| `city-m1-junctions.png` | Nothing serious — but the convex hull boundary is still a coin |
| `city-m1-large-junctions.png` | A ruled diameter straight across the image through the centre |
| `city-real-5k-junctions.png` | The centre singularity, laid bare. This image is the diagnosis |

Minor but sloppy: the third caption line is **clipped by the right image edge** in every large render
(`... TALLEST DJANGO/__MAIN__.PY H=` — value cut off; same in the 5k render).

## 7. The highest-value change to the image

**Delete the polar layout. Take the geometry from `docs/design/accretion/` — which is already in this
repo and is already right — and render it with the current dark palette.**

Put the two side by side at 180px. `accretion/large.png` has a ragged, lobed, deeply non-convex
footprint with tendrils following roads out into the black; districts are irregular interlocking
blobs; there is no centre and no radius anywhere in it. It reads as a place at a glance. Its only
fault was that it was too bright and too saturated — and that is precisely the fault that has already
been fixed in the current renderer.

The two halves of the answer already exist in the repository, in different directories. Round 1 said
this. Ship `accretion` geometry + current tone and the squint test is passed in one cycle.

Then, and only then, the second change:

**Make height visible.** Give roofs a tonal ramp across the full range up to ~#C8C8C0, add a dark
offset shadow proportional to height, and widen the footprint distribution ~10x within a block so
mass varies. Right now the tallest building in a 7000-file repository is indistinguishable from its
neighbours and is identified only by a text label. That is the whole point of the map, and it is the
one thing the map does not do.

**Do not answer this review with another palette change.**
