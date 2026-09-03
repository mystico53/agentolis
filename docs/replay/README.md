# Replay recordings

Three real Claude Code sessions from this machine's own `~/.claude/projects`,
replayed over a real generated city and recorded headlessly. No window, no GPU:
`polis_render::frame::FrameRenderer` renders each frame, `polis_render::gif`
encodes the sequence.

All three are 1342x1200 — a 1100x1100 map, a caption strip under it and PRD
§6.2's status rail to the right of it — 480 frames at 4 cs, so 25 fps and 19.2
seconds, and use the `timed` trail notation. Every one uses the replay clock's
idle-gap compression (`DEFAULT_IDLE_GAP_CAP`, 2 s) — without it these sessions
are 13 to 63 hours of mostly dead air.

The M2 recordings were 1100x1200: the rail is **added to** the width rather than
taken out of the map, so the map square is the same size in both and the
measurements below compare like with like.

| file | session | repository | shows |
|---|---|---|---|
| `focus-qurio-services.gif` | `29c2fc6f` | qurio-toolset | one agent working inside a territory it has claimed |
| `delegation-agentolis.gif` | `6f51089f` | agentolis | 59 subagents fanning out across 8 crates |
| `editing-qurio-components.gif` | `9cab97d7` | qurio-toolset | a single-threaded editing session at normal density |

**These three were re-recorded after the placement, trail, attention and rail
work.** The headline result is [the colour census](#the-colour-channel-measured):
red is now drawn where it belongs and is legible at the mark, and it is still
far too quiet to read from across the room.

---

## `focus-qurio-services.gif` — a claimed territory

63.3 hours, 14 979 events across 116 transcript files, compressed to 77.9
minutes; this is minutes 54.9–61.9 of that compressed timeline — **the same
window the M2 recording used**, so the only thing that changed between the two
files is the renderer. The city is 1592 buildings in 12 006 districts; 261 of
the 275 paths the session touched land on a real building.

The agent has converged on `src/services` and PRD §6 territory inference has
made a claim, so the §10.3 layer-3 cloud is drawn: three nested iso-contours
with a hatched interior. Inside it are edit glyphs, a write glyph, revisit
rosettes on two buildings, and scaffolding on the files with uncommitted diff.
The trail leaves the cloud and comes back several times — that is the §12
backtracking notation doing its job, and 90% of frames now carry at least one
visible return leg.

New since M2: the shell calls are on the map. This session ran 695 of them
(416 `PowerShell`, 279 `Bash`) and **131 of them failed**; under the old rule
every one of those drew on the repository-root district, on one pixel, which is
the same as not drawing them. Red marks now appear in 226 of the 480 frames,
against 17 before.

This is the one to watch first. It has the most going on and the most motion:
a median of 3 322 of the map's 1.21M pixels change per frame, and only 11% of
frames change fewer than 200.

## `delegation-agentolis.gif` — the fan-out

Polis's own repository, and Polis's own build session: 31.5 hours compressed to
25.9 minutes, recorded over the last 18.6 seconds of it. 175 buildings, 2781
districts; 151 of 203 touched paths land on the city.

**This session is the one being recorded from.** It was live while this file was
made — several agents were appending to its transcript during the run — so its
compressed duration moved between 24.5 and 25.9 minutes across successive
recording attempts, and a window given as a *fraction* of the timeline lands
somewhere different every time. That is why this recording alone is not on the
M2 window: the M2 window cannot be reproduced on a transcript that is still
being written. The camera is left on auto (extent 39.2 of a 178.1 city), which
now keeps the orchestrator in frame; the M2 `POLIS_EXTENT=30` pin is no longer
needed.

The tether fan is at its widest for almost the whole recording: 59 workers
running at once out of the 74 the session spawned, each tethered back to the
orchestrator, spread over
`polis-layout`, `polis-repo`, `polis-app`, `polis-ingest`, `polis-render`,
`polis-world`, `polis-events` and `docs`, with a 128-kernel territory cloud over
`.claude/worktrees`. An amber needs-decision pin stands in **every one of the
480 frames** — this session ends waiting on a human, and it is the only one of
the three where the attention layer is continuously visible.

It is still close to a still image: 61% of frames change fewer than 200 of the
map's 1.21M pixels, against 67% for the M2 file on the same measure, and 1 855
of its operations land in the last 96 frames. The cause is the ingest artefact described below — 90 subagent
transcripts read in file order with a running-maximum timestamp — not the
pacing, and it is **not fixed**. `still-delegation-fan.png` still carries almost
as much as the animation does.

## `editing-qurio-components.gif` — an ordinary session

13.8 hours, 4604 events across 35 transcript files, compressed to 19.1 minutes;
this is minutes 5.7–14.4 — again **the M2 window**, unchanged. 96 of 97 touched
paths land on the city.

Included because it is *typical* rather than spectacular: one thread, no
concurrent workers, no territory claim, a few marks at a time on a base map that
happens to be framed better than either of the others. It is the honest answer
to "what will this look like on a normal afternoon".

It is also the clearest demonstration that the notation works, because there is
room around each mark. One frame (245) carries all six of PRD §10.1's glyphs
within a few hundred pixels — a hollow circle, a barred circle, a filled square,
a filled triangle, concentric circles and a delegate rosette — and one of the
hollow circles is red. This session's 37 failures are 29 `PowerShell`, 6 `Bash`,
one browser call and one workflow call: **not one of them carries a file path**, and before this
round not one of them was drawn anywhere the eye could find. Red now appears in
86 of the 480 frames, against 0 before.

It got *steppier*, not smoother: 19% of frames change fewer than 200 pixels,
against 9% in M2, and the median per-frame change fell from 753 pixels to 505.
Same window, same session, so this is the renderer, not the schedule.

---

## The colour channel, measured

The operator's verdict on the M2 recordings was *"yeah color is all the same,
teal"*, and the measurement agreed: across all 1 440 frames, teal was 93–99% of
saturated agent-band pixels, red was at most 0.1%, amber 0.0%, and the >168
attention band was 0.000% of map area in every frame. Those sessions contain
544 real tool failures.

**The colour code was never wrong.** `is_error` parses, `Outcome::from_is_error`
maps, `apply::settle` assigns, `outcome_ink` renders. What was wrong is where
the failures were drawn. 374 of 544 failures are on `Bash` or `PowerShell`,
which carry no `file_path`; only 24 are on a tool that names one. Those shell
calls resolved their path to the **repository root**, the root district exists,
so every one of them drew at a single point in the middle of the map. Six
thousand marks on one pixel is not a mark. See `polis-world/src/place.rs`.

### The same measurement, before and after

Every frame of all three recordings, agent band (channels 97–168), saturated
pixels (chroma ≥ 0.18) bucketed by nearest hue. The "before" column is measured
off the M2 files themselves, recovered from git — not recomputed — so the two
columns are the same classifier over two real artefacts.

| of saturated agent-band px | `focus` was → now | `delegation` was → now | `editing` was → now |
|---|---|---|---|
| **teal** | 99.98% → **99.86%** | 99.95% → **99.75%** | 100.00% → **99.75%** |
| **red** | 0.02% → **0.14%** | 0.05% → **0.23%** | 0.00% → **0.25%** |
| **amber** | 0.00% → **0.00%** | 0.00% → **0.02%** | 0.00% → **0.00%** |
| frames carrying any red | 17/480 → **226/480** | 0/480 → **480/480** | 0/480 → **86/480** |
| attention band, % of map area | 0.0000% → 0.0000% | 0.0000% → **0.0112%** | 0.0000% → 0.0000% |
| frames carrying an attention mark | 0/480 → 2/480 | 0/480 → **480/480** | 0/480 → **0/480** |

**Read the top row honestly: teal still owns 99.8% of the saturated agent band.**
The reason is not the outcome channel. It is that trail and tether ink outweigh
the marks by area — 41% + 35%, 88% + 8% and 74% + 8% of the agent band in the
three files. Measured over **mark ink only** (the three outcome colours plus the
two agent bodies), which is the channel this work changed:

| of mark ink | `focus` was → now | `delegation` was → now | `editing` was → now |
|---|---|---|---|
| **teal** | 90.18% → 92.81% | 55.80% → **33.91%** | 98.10% → 91.89% |
| **red** | 0.05% → **0.57%** | 0.00% → **5.88%** | 0.00% → **1.37%** |

And over the marks themselves rather than their pixels, across each whole
session (`polis-render`'s `colour_census`), failure is now 5.2% / 9.9% / 9.2%
of every mark drawn.

So: **the colour channel now carries information, and the trail layer buries
it.** One mark in ten to twenty is red; one saturated pixel in four hundred is.
Whether that is a trail-weight problem or a mark-weight problem is the next
question, and it is a §10.3 dynamic-range question, not a colour one.

The two tables above were measured off the GIF files, which is what makes the
"before" column possible at all — the M2 artefacts still exist and can be
classified again. The same census also runs in-tree against the live renderer:

```sh
POLIS_SESSION=29c2fc6f POLIS_SPAN=whole cargo test --release -p polis-render \
  --test colour_census -- --ignored --nocapture
```

which prints the same old-rule-vs-new comparison from **one binary**, so neither
the trail notation nor `MARK_TTL` can move between the two columns: mark-layer
ink goes teal 98.85% / red 0.74% under the old rule to teal 97.83% / red 1.72%
under the new one on `29c2fc6f`, and 88.23%/0.41% to 85.77%/**5.60%** on
`6f51089f`.

### Every operation is now placed

`polis-world`'s `placement_census` walks the four rungs of `place::site_of` —
the op's own path, a shell call's working directory as a district, the acting
agent's position, and PRD §6.2's status rail — for every operation of a whole
session:

| | `29c2fc6f` | `6f51089f` | `9cab97d7` |
|---|---:|---:|---:|
| operations | 3 855 | 10 735 | 1 355 |
| placed **now** | 3 826 (99.2%) | 10 735 (100%) | 1 353 (99.9%) |
| placed **before** | 2 868 (74.4%) | 9 043 (84.2%) | 1 101 (81.3%) |
| on the rail, not the map | 29 | 0 | 2 |
| failures placed | 152/152 | 185/185 | 37/37 |

The "before" column is the old rule — `op.path` resolving straight to geometry —
evaluated in the same binary. Note what it does **not** say: the old rule drew
most shell calls. It drew them all in the same place. The rungs are what moved,
not the count: 6 804 of one session's operations resolved to the repository root
and now resolve to the agent instead.

Dropped operations, by tool, are now **zero** in every session. What cannot be
placed is counted and listed in the rail rather than discarded, and across all
1 440 frames of these three recordings the rail's row area is empty in every
frame — every operation these sessions ran found a home on the map.

### All six glyphs now occur

PRD §10.1 has six operation glyphs and the M2 recordings showed three. Marks
drawn across a whole session, by glyph:

| glyph | `29c2fc6f` | `6f51089f` | `9cab97d7` |
|---|---:|---:|---:|
| read — hollow circle | 575 | 22 | 108 |
| edit — barred circle | 639 | 32 | 207 |
| write — filled square | 177 | 21 | 192 |
| **run — filled triangle** | **173** | **126** | **206** |
| verify — concentric circles | 79 | 10 | 89 |
| delegate — dot and satellites | 10 | 21 | 54 |

`run` is now the most-drawn glyph in `6f51089f` (54% of its marks) and the
second-most in `9cab97d7`. It is the busiest thing an agent does and it was
invisible.

---

## What a replay can and cannot show

**The three GIFs above are the M2 artefacts and predate everything in this
section.** They are kept as the record of what was measured. The counts below
come from the operator's real sessions and from the current renderer.

### The attention layer (PRD §11)

The measured starting point: across all 1 440 frames of the three recordings,
the attention band (channels 169–255, PRD §10.3) was **0.000% of map area in
every frame**. Not one mark, in any of them. That was not a colour bug. PRD
§11.2 sources the state from hooks — `PermissionRequest`, `Elicitation`,
`TeammateIdle` — and a replayed transcript is Channel D, so nothing could raise
a mark at all. A recording that structurally cannot show the state the product
exists for is not evidence that the state works.

Three rules now reconstruct as much of it as a transcript carries. All three are
tagged `DecisionSource::is_reconstructed()`, because PRD §17 requires anything
that drives an alert to come from an authoritative channel and a recording is
not one.

| source | what it is | onset | in `29c2fc6f` / `6f51089f` / `9cab97d7` |
|---|---|---|---|
| `AskUser` | an `AskUserQuestion` or `ExitPlanMode` call — a tool that blocks on a human by construction | exact | 7 / 0 / 1 calls, waits 42 s to 2 h |
| `TurnEnded` | a **main** agent ended its turn (`stop_reason: end_turn`) with no tool call, and no human has replied | exact | 62 / 38 / 6 waits, medians 5.2 / 12.6 / 27.3 min |
| `Rejected` | `toolDenialKind`, `interruptedMessageId`, or a `[Request interrupted by user]` record | **late** | 20 / 8 / 0 |

`TurnEnded` is the one that matters: it is the transcript's form of the
`idle_prompt` / `agent_needs_input` notification §11.2 lists, it is exact in both
onset and duration, and every session in this corpus has it.

Measured end to end, with `polis-render`'s `replay_measure` harness driving a
96-frame recording of each session at the derived pacing:

| | `29c2fc6f` | `6f51089f` | `9cab97d7` |
|---|---:|---:|---:|
| frames with an attention mark — **was 0.0% in all 1 440 M2 frames** | 24.0% | 6.2% | 32.3% |
| peak attention-band share of map area — **was 0.0000%** | 0.0289% | 0.0303% | 0.0372% |
| trail steps held at the recorded moment — was 29 / 192 / 1 | 192 | 192 | 192 |
| frames showing at least one **return** leg | 90% | 46% | 32% |
| most return legs in one frame | 28 | 150 | 86 |
| frames changing fewer than 200 of 1.32M pixels | 3% | 32% | 47% |

Reproduce with:

```sh
POLIS_SESSION=29c2fc6f cargo test -p polis-render --release \
  --test replay_measure -- --ignored --nocapture
```

`6f51089f` is the weakest of the three on every column, and the reason is not
the attention layer. Its 90 subagent transcripts are read in file order with a
running-maximum timestamp, so **21 193 of its 21 431 live events land inside a
ten-second stretch at the end of the compressed schedule**. Whatever window the
planner picks either crawls through that pile or steps over it. That is an
ingest-side artefact (`polis-ingest`'s offline full read), not a pacing one, and
it is the single biggest thing standing between this session and a watchable
recording. Re-measured this round: **1 855 of its operations still land in the
last 96 of 480 frames.** Unchanged.

**That table is measured on the derived-pacing window, which is not the window
two of the three published GIFs use.** The distinction matters most for the
attention layer, because `pacing::plan` deliberately weights "waiting on you"
moments and so steers *towards* them. On the files actually published here:

| frames carrying an attention mark | `focus` | `delegation` | `editing` |
|---|---:|---:|---:|
| derived-pacing window, 96 frames | 24.0% | 24.0% | 32.3% |
| **the published 480-frame recording** | **2/480** | **480/480** | **0/480** |

`delegation-agentolis` ends waiting on a human and carries an amber pin in every
frame; `editing-qurio-components` carries none at all, because on its M2 window
nobody was ever waiting. Neither number says anything about whether the
attention layer works — see the two sections below for why a replay cannot be
evidence about that. It does mean **the published `editing` recording is not a
demonstration of the attention layer**, and should not be shown as one.

### What a replay still cannot show, at all

1. **Contention (§11.3) — the highest-ranked state, and the one where work is
   being destroyed.** It is a relation between two *threads* over one logical
   path, and a thread is a session (`ThreadId::of_session`). One replayed
   session is one thread, so the claim table's rule — *a second live claim on the
   same path from a **different thread*** — can never fire, however many workers
   the session has. Measured: in `6f51089f`, the 69-worker run, **77 logical
   paths were written by more than one worker and zero pairs landed inside the
   30 s claim TTL**. `9cab97d7` contains exactly one true collision — two
   subagents wrote `src/components/custom/settings/settings.css` 4.7 s apart —
   and it does not render, because both workers belong to one thread. **A replay
   is not evidence about contention.** Only a live multi-session fleet is.

2. **`Done` (§11.2 (b)), both halves.** Sourced from `Stop` / `SessionEnd`,
   which are Channel B. A transcript never says "the session finished"; it just
   stops, which is not knowable at the time. So neither *done, verified* nor
   *done, unverified* appears in a replay — two of the renderer's four attention
   shapes are unreachable here.

3. **`Elicitation`, `TeammateIdle`, and the `permission_prompt` notification.**
   No transcript record corresponds to any of them.

4. **The onset of a real permission prompt.** `PermissionRequest` fires the
   instant Claude asks. A transcript records only the *answer*, so a `Rejected`
   mark arrives late by however long the operator took to decide, and
   `DecisionSource::onset_is_exact()` returns `false` for it.

**Read a calm replay as "this session had none of these states", never as "the
attention layer works".**

### Trails and thrashing (PRD §12)

Two separate defects, both fixed, both measured on the 2 765 revisits in these
three sessions.

*The return leg was drawn on top of the outbound one.* `A → B → A` pushes three
stops and draws two segments — and the two segments are the same two points in
the opposite order, so six passes over one building drew as one line. Watching
the recordings you saw marks accumulate and never saw an agent come **back**;
the revisit rosette said "this file was revisited a lot" without ever showing
the oscillation. Repeated legs now bow apart, alternating sides and widening
with each pass, so a round trip draws as a lens and an oscillation as a spindle
— `still-thrashing-lens.png` is the four cases side by side at the size they are
really drawn. Measured on the recordings: 90% / 46% / 32% of frames now carry at
least one visible return leg, up to 28 / 150 / 86 of them in a single frame.
Before, every one of those was drawn on top of its outbound leg.

*The trail was too short to contain a backtrack.* The number of trail steps
between two consecutive visits to one path has a median of 9–31 and a 90th
percentile of 97–418, and the time between them exceeds five minutes on a third
to a half of all revisits. Against that, `TRAIL_CAP` 64 with a 300 s
`TRAIL_TTL` held **51.8% / 46.3% / 69.5%** of real round trips; 192 with 900 s
holds **67.6% / 61.8% / 80.2%**. 256 buys half a point, so 192 is where it sits.

### Pacing (PRD §13)

The M2 recordings were paced by a caller-supplied constant: a fixed 45-second
window of the compressed timeline cut into N equal slices. Nothing checked the
result, and the result was doing all the work — `focus-qurio-services` at ~0.9
world-seconds per frame reads as alive, while `delegation-agentolis` spent 43%
of its frames changing fewer than 200 of 1.32M pixels.

`polis_render::pacing` derives it from the schedule instead. Frames are cut
every `MASS_PER_FRAME` **live** events — records that can move the live layer,
which excludes the assistant text and thinking blocks that dominate a
transcript's record count — and every step is clamped to `[40 ms, 1000 ms]`: no
slower than real time, and never past PRD §11.4's 400 ms arrival pulse. A
"waiting on you" moment carries no events at all, so it is weighted as
`DECISION_DWELL_FRAMES` frames of mass; otherwise every window search walks
straight past the state the product is for. `Pacing::verdict()` names the two
failure modes out loud — *mostly dead air* and *denser than the frame budget* —
instead of shipping them.

**At the 480-frame budget these recordings use, the derived plan rejects itself
on all three sessions.** Measured: 410/480, 478/480 and 334/480 frames land on
the step ceiling, and `verdict()` returns *"mostly dead air"* for every one. The
mechanism is arithmetic rather than a bug: a frame may advance at most
`MAX_STEP_MS`, so N frames can never span more than N seconds of playback, and
whenever the schedule's mass runs out before N frames are placed the planner has
no option but to widen the window into the emptiness at the ceiling rate. More
frames therefore buys more dead air on a clumped schedule, which is the opposite
of what a bigger frame budget should do. At 96 frames the same planner picks a
tight, well-judged window on the same sessions.

Two of the three files above are therefore on their M2 windows via
`POLIS_FROM`/`POLIS_TO`, which also keeps the colour comparison controlled; the
third is on a hand-picked window for the reason given in its own section. The
planner is right that these schedules are mostly dead air. It is the *window
width forced by the frame count* that needs a second look — a cap on how far the
window may widen once the mass is exhausted, or a frame count derived from the
schedule rather than supplied by the caller.

---

## PRD §17: what is on screen that does not change a decision

§17 asks the question of *every* visual element, so here it is asked of these
frames. Four things fail it.

**`GEN 247` in the caption.** The snapshot generation counter. It changes every
frame, which makes it one of the most eye-catching things in the strip, and no
value of it has ever changed what an operator would do. It is a debugger's
watch expression that ended up in the product. Cut it.

**`PLACED 8 928` in the rail.** How many operations found a home on the map is
the metric *this round's work* is judged by, and it earned its place while that
was in doubt. It is not an operator's number — they cannot act on it, and it
only ever goes up. `RAILED` and `FAILED` are the two that matter, and `RAILED`
matters only when it is non-zero. Keep the counters that can be zero; drop the
one that cannot.

**The status rail's 242 pixels.** 18% of the canvas width, and in all 1 440
frames of these three recordings it holds a two-line header, the word `ALL
PLACED`, and three counters. That is the correct outcome — nothing was stranded
— but a permanent 18% reservation for a column that is almost always empty is
the wrong trade at this zoom. It should collapse to its header when there are no
rows, and expand when there are.

**The tether and trail ink, at its current weight.** This one is not chrome and
cannot simply be cut, but it fails the spirit of §17 by *starving* something
that does change a decision. Trail plus tether is 76%, 96% and 82% of the agent
band in the three files, against 0.14–0.25% for red. A failure is one mark in
ten to twenty and one saturated pixel in four hundred. PRD §10.3 says to spend
the dynamic range on layers 4–5; inside layer 4 the same argument applies, and
the outcome channel is currently losing it to the history channel. The fix is a
weight, not a colour.

Passing §17, for the record: the six glyphs (shape is what the operator decodes
once they turn their head), the three outcome colours, the amber pin, the
revisit rosette, the tether fan (it is the only thing that says "this is one
thread, not thirty"), the district labels, and `n WAITING`.

`T+01:31:35`, `53%` and `TIMED TRAIL` are recorder chrome rather than product
surface — a replay artefact needs a clock and a progress fraction, and the trail
name exists so two notations can be told apart in an A/B. They should not appear
in the live window.

---

## Stills

Frames a GIF cannot show. `still-thrashing-lens.png` is a synthetic notation
sheet; of the rest, three are lifted pixel-for-pixel out of the animations above
and two are separate renders at framings the GIFs do not use.

**Except `still-thrashing-lens.png`, these stills predate the re-recording and
the frame numbers below no longer index the current GIFs.** They are still
accurate pictures of the notation — none of the marks they show changed — but
they were cut from the M2 files, so they carry no status rail, no shell-call
glyphs and no red. Treat them as notation references, not as evidence about the
colour channel.

| file | what it is for |
|---|---|
| `still-thrashing-lens.png` | PRD §12's thrashing notation at the size it is really drawn: one, two, four and six passes over the same pair of buildings, with the revisit rosette beside each. One pass is a line; every further pass bows to the other side, so a backtrack is a lens and an oscillation is a spindle. Before this, all four rows drew as row one. Regenerate with `POLIS_OUT=<dir> cargo test -p polis-render --release -- --ignored --nocapture thrashing_sheet`. |
| `still-notation-detail.png` | 2x crop. The clearest look at the notation: edit glyphs, a write glyph, the agent's ring, two revisit rosettes, the hatched cloud, and solid vs. dashed trail. |
| `still-delegation-fan.png` | The tether fan at its widest, frame 377 of `delegation-agentolis.gif`. |
| `still-territory-cloud.png` | Frame 258 of `focus-qurio-services.gif`, full frame, for the cloud against the whole map. |
| `still-closeup-agentolis.png` | The agentolis session re-rendered at 1.4x zoom. Glyph, rosette, scaffolding and cloud at a size where each is unambiguous. |
| `still-two-lobes.png` | The `29c2fc6f` session at a 20x wider timescale: attention split into two separate territories with a trail spanning the map. |

---

## Redaction

Nothing here shows file contents, secrets, or an email address. Paths and
filenames are the map, and they are the point of it.

The renderer draws four kinds of text, one more than it did at M2:

1. **District and monument labels** — directory and file names, from the city.
2. **The caption strip** — the recorder's own `T+hh:mm:ss` clock, the thread /
   worker / file counters, and the glyph and colour legend. All fixed strings
   and integers.
3. **The status rail's counters** — `PLACED`, `RAILED`, `FAILED`, and either
   `NOW: ALL PLACED` or a row per thread that has operations with nowhere to go.
4. **A status-rail row's label**, which is new, and is the one to watch.

That label is `Thread::title`, and its sources are the transcript's `ai-title`,
`custom-title` and session slug — model- or user-written summaries of the
conversation, not paths. In this corpus they are innocuous
(`optimize-settings-menu`, `missing-inference-signals`), but they are unbounded
conversation-derived text and the old blanket claim that "no prompt or message
body reaches a pixel" is no longer true by construction.

Checked rather than assumed: **the rail's row area is empty in all 1 440 frames
of all three recordings** — 0 pixels of ink above the counter block, in every
frame — because every operation these sessions ran was placed on the map. So no
title is drawn in any published frame here. A session that *does* strand an
operation would print one, and anyone recording a session for publication should
either confirm the rail is showing `ALL PLACED` or expect a conversation summary
on screen.

## How these were made

The recorder is the `#[ignore]`d `record_a_real_session` test in
`polis-render/src/frame.rs`, driven by environment variables:

```sh
POLIS_SESSION=29c2fc6f-6cfb-4fc8-a2d0-5dfc886f87cc \
POLIS_OUT=/some/dir POLIS_TRAIL=timed \
POLIS_FRAMES=480 POLIS_PIXELS=1100 POLIS_FROM=0.705 POLIS_TO=0.795 \
cargo test --release -p polis-render record_a_real_session -- --ignored --nocapture
```

`POLIS_FROM`/`POLIS_TO` are fractions of the *compressed* timeline and force a
uniform sampling of that span — which is how the three GIFs above were made.
Left unset, the recorder no longer picks a fixed window at all: it calls
`polis_render::pacing::plan`, which chooses the window **and** the per-frame
step from the schedule's own live-event density and prints its verdict. Prefer
that; `POLIS_FROM`/`POLIS_TO` is now the escape hatch rather than the path.

`polis_render::gif` writes every frame in full, which is correct and large: this
round's three recordings are 56, 83 and 93 MB as written. The base map is static
by construction — 99.7%, 100.0% and 99.9% of pixels are unchanged from one frame
to the next — so the files here were re-encoded with frame differencing: every
unchanged pixel replaced by a transparent palette index, disposal 1, each frame
cropped to the bounding box of what moved, and the global colour table carried
through byte for byte so the §10.3 contrast bands are untouched. 232 MB became
6.5 MB (3.6 / 1.4 / 1.4), 15x to 65x.

Each output was verified pixel-exact against the renderer's own output by two
independent decoders (Pillow and ImageMagick `-coalesce`), frame for frame, max
channel difference **0**. The verification is not ceremony — it caught three
separate silent corruptions on the way to that number, and every one of them
produced a file that opened and animated:

- **Pillow's animated-GIF writer cannot be used for this.** It runs its own
  inter-frame delta and crops to the bbox of `subtract_modulo(frame, previous)`.
  Fed frames that are already differenced, that subtraction is between two
  transparency-masked images rather than two composited ones, and the bbox drops
  pixels that mattered. Max channel difference 200. The container here is
  therefore assembled by hand and Pillow is used only to LZW-compress each
  single image block, which is spliced out of a one-frame GIF.
- **`Image.convert("P")` on an already-palettised image re-quantises**, silently
  substituting a different palette. Read `getpalette()` before any `seek`, and
  recover indices with an explicit RGB→index lookup.
- **Pillow interlaces by default.** An interlaced data stream spliced under an
  image descriptor that declares progressive order decodes with its rows in the
  wrong places — a one-row shift, which is exactly as wrong as it is easy to
  miss. `interlace=False`, and reject the block if the descriptor's 0x40 bit is
  set.
- The graphic control extension's flags byte is
  `(disposal << 2) | (user_input << 1) | transparent`. Disposal 1 is `0x05`;
  `0x09` is disposal 2, restore-to-background, which wipes the map every frame.

Do not use ImageMagick's own `-layers OptimizeTransparency -layers OptimizeFrame`
for this: on these frames it produces a 22x smaller file whose base map decodes
as solid black, in ImageMagick's own `-coalesce` as well as in Pillow.

That re-encode is a post-process, not part of the crate. Given how many ways
there are to get it silently wrong, if it is worth keeping it belongs in
`gif.rs` as a `write_differenced` with the verification as a test, not as a
script someone has to rediscover.
