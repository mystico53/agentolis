# Replay recordings

Three real Claude Code sessions from this machine's own `~/.claude/projects`,
replayed over a real generated city and recorded headlessly. No window, no GPU:
`polis_render::frame::FrameRenderer` renders each frame, `polis_render::gif`
encodes the sequence.

All three are 1100x1200, 480 frames, 20 fps, 24 seconds, and use the `timed`
trail notation. Every one uses the replay clock's idle-gap compression
(`DEFAULT_IDLE_GAP_CAP`, 2 s) — without it these sessions are 13 to 63 hours of
mostly dead air.

| file | session | repository | shows |
|---|---|---|---|
| `focus-qurio-services.gif` | `29c2fc6f` | qurio-toolset | one agent working inside a territory it has claimed |
| `delegation-agentolis.gif` | `6f51089f` | agentolis | 32 subagents fanning out across 8 crates |
| `editing-qurio-components.gif` | `9cab97d7` | qurio-toolset | a single-threaded editing session at normal density |

---

## `focus-qurio-services.gif` — a claimed territory

63.3 hours, 14 979 events across 116 transcript files, compressed to 77.9
minutes; this is minutes 54.9–61.9 of that compressed timeline. The city is
1582 buildings in 12 021 districts; 260 of the 275 paths the session touched
land on a real building.

The agent has converged on `src/services` and PRD §6 territory inference has
made a claim, so the §10.3 layer-3 cloud is drawn: three nested iso-contours
with a hatched interior. Inside it are edit glyphs, a write glyph, revisit
rosettes on two buildings, and scaffolding on the files with uncommitted diff.
The trail leaves the cloud and comes back several times — that is the §12
backtracking notation doing its job.

This is the one to watch first. It has the most going on and the most motion.

## `delegation-agentolis.gif` — the fan-out

Polis's own repository, and Polis's own build session: 29.5 hours compressed to
23.2 minutes, recorded over the last 25 seconds of it. 166 buildings, 2878
districts. This session was still running while it was recorded, so its
transcript — and the city, which includes `target/` — moved under the recorder;
the numbers here are from the run that produced this file.

The first few seconds are one agent working alone. Then the workflow spawns its
workers and the tether fan appears — at its widest, 33 of 69 workers running at
once, each tethered back to the orchestrator, spread over `polis-layout`,
`polis-repo`, `polis-app`, `polis-ingest`, `polis-render`, `polis-world`,
`polis-events` and `docs`. Scaffolding marks appear on every file being grown.

The fan-out is the event worth seeing. What follows it is close to a still
image — 43% of the frames change fewer than 200 pixels — so `still-delegation-fan.png`
carries almost as much as the animation does. The camera is pinned wide
(`POLIS_EXTENT=30`) rather than left on auto, because the auto framing zooms in
far enough to crop the orchestrator out of the picture, which turns the fan into
a bundle of parallel lines arriving from off-screen.

## `editing-qurio-components.gif` — an ordinary session

13.8 hours, 4604 events across 35 transcript files, compressed to 19.1 minutes;
this is minutes 5.7–14.4. 96 of 97 touched paths land on the city.

Included because it is *typical* rather than spectacular: one thread, no
concurrent workers, no territory claim, a few marks at a time on a base map that
happens to be framed better than either of the others. It is the honest answer
to "what will this look like on a normal afternoon".

---

## Stills

Frames a GIF cannot show — the first three are lifted pixel-for-pixel out of the
animations above, the last two are separate renders at framings the GIFs do not
use.

| file | what it is for |
|---|---|
| `still-notation-detail.png` | 2x crop. The clearest look at the notation: edit glyphs, a write glyph, the agent's ring, two revisit rosettes, the hatched cloud, and solid vs. dashed trail. |
| `still-delegation-fan.png` | The tether fan at its widest, frame 377 of `delegation-agentolis.gif`. |
| `still-territory-cloud.png` | Frame 258 of `focus-qurio-services.gif`, full frame, for the cloud against the whole map. |
| `still-closeup-agentolis.png` | The agentolis session re-rendered at 1.4x zoom. Glyph, rosette, scaffolding and cloud at a size where each is unambiguous. |
| `still-two-lobes.png` | The `29c2fc6f` session at a 20x wider timescale: attention split into two separate territories with a trail spanning the map. |

---

## Redaction

Nothing here shows file contents, secrets, or an email address. The renderer
draws exactly three kinds of text: district and monument labels (which are
directory and file names), the footer counters, and the glyph legend. No tool
input, tool output, prompt, or message body reaches a pixel. Paths and filenames
are the map.

## How these were made

The recorder is the `#[ignore]`d `record_a_real_session` test in
`polis-render/src/frame.rs`, driven by environment variables:

```sh
POLIS_SESSION=29c2fc6f-6cfb-4fc8-a2d0-5dfc886f87cc \
POLIS_OUT=/some/dir POLIS_TRAIL=timed \
POLIS_FRAMES=480 POLIS_PIXELS=1100 POLIS_FROM=0.705 POLIS_TO=0.795 \
cargo test --release -p polis-render record_a_real_session -- --ignored --nocapture
```

`POLIS_FROM`/`POLIS_TO` are fractions of the *compressed* timeline. Left unset,
the recorder picks the busiest window itself — on every session in this corpus
that lands in the final seconds, because subagent transcripts arrive in one
burst at the end (see the review notes for why that is a problem).

`polis_render::gif` writes every frame in full, which is correct and large: these
three recordings are 39, 95 and 88 MB as written. The base map is static by
construction, so the files here were re-encoded with frame differencing — every
pixel that did not change replaced by a transparent index, disposal 1, the
global colour table carried through byte for byte so the §10.3 contrast bands
are untouched. Each output was verified pixel-exact against the renderer's own
output by two independent decoders (Pillow and ImageMagick `-coalesce`), max
channel difference 0. 223 MB became 10.4 MB.

Do not use ImageMagick's own `-layers OptimizeTransparency -layers OptimizeFrame`
for this: on these frames it produces a 22x smaller file whose base map decodes
as solid black, in ImageMagick's own `-coalesce` as well as in Pillow.

That re-encode is a post-process, not part of the crate. If it is worth keeping,
it belongs in `gif.rs` as a `write_differenced`.
