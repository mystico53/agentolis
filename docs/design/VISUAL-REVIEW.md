# Polis — Visual Review (fresh eyes, no implementer claims read)

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
