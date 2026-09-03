# Final review — does Polis do what it claims?

A fresh pair of eyes, judging by looking. No code changed. Read alongside PRD §10.3,
§11, §17.

**Verdict in one line: the city is real, the drill-down is excellent, and the product
thesis fails at the only distance that matters. On a live map with a real agent
waiting on a human, 0.5 % of the ink above L100 is the amber "needs decision" mark
and 74.7 % is decoration.**

---

## 1. What I actually ran

| | |
|---|---|
| Built | `cargo build --release -p polis-app` (27.98 s, warm) |
| Base map | `polis -C C:\coding\agentolis map` — window opened, screenshotted, resized |
| Empty live | `polis -C <scratch> watch` on a 9-file scratch repo |
| **Real agent** | Claude Code v2.1.248 (Haiku 4.5) in that scratch repo, project-local `.claude/settings.json` written by `polis connect -y`. Read two files, requested Bash permission twice, one tool failure. Trust dialog, permission prompt, approval and failure all exercised. |
| **Real fleet** | `polis -C C:\coding\agentolis watch` against the operator's own live session — 1 thread, **87 workers, 7 510 calls, 136 failures**, one live `WAITING ON YOU` |
| Measured | colour census and luminance histogram of the live map pane; 60×60 squint; frame-to-frame churn over 4.5 s; process CPU/RSS |
| Reviewed | every PNG and GIF in `docs/` and `docs/replay/` (51 unique images, 3 × 480-frame GIFs) |

Screenshots and measurement scripts: `%TEMP%\claude\C--coding-agentolis\6f51089f-…\scratchpad\review\`.

Green, unchanged, on the tree as I found it:

```
cargo fmt --all -- --check     → exit 0, no output
cargo clippy --workspace --all-targets → Finished `dev` profile ... in 2.83s   (zero warnings)
cargo test --workspace         → TOTAL passed: 1251  failed: 0
```

---

## 2. Can you tell at a glance that something needs you?

**No.** This is the entire product and it does not work yet.

The measurement, taken on the live map pane (1050 × 1092 px = 1 146 600 px) at the
moment one thread was genuinely blocked on a human decision:

| what | pixels | % of pane |
|---|---|---|
| neutral ink above L100 — tethers, label plates, building edges | 34 213 | 2.98 % |
| red — failure marks and the alarm ring | 11 451 | 1.00 % |
| **amber — `needs decision`, the state the product exists for** | **243** | **0.021 %** |

**Share of all ink above L100: 74.7 % decoration, 24.0 % red, 0.5 % amber.**

PRD §10.3 asks for layers 1–2 in the bottom fifth of the contrast range and the rest
spent on layers 4–5. What is actually on screen is the inverse: three quarters of the
top of the range goes to the base map's labels and to the delegation tethers.

Downsampled to the project's own 60 × 60 squint metric: **57 red-hot pixels, 1 amber
pixel.** The secondary state out-inks the primary by 57 : 1. At 180 px the live map
reads as *a place with a rash* — forty small red rings scattered over half the city,
one big red alarm gear on the left, a white fan across the middle, and a single yellow
speck you will not find.

`docs/attention/salience-before-after.png` is honest and its work is real: the alarm
ring took the failing case from 2 hot px to 54 and that survives the box filter. But
the same treatment was never given to `needs decision`, and §11.1 ranks contention
*above* it — so the state you must act on is both quieter and lower in the list than
the state you mostly cannot act on.

### The rail is not a substitute

With the real fleet the rail said:

```
ATTENTION  33
CONTENTION  same file · two workers of one agent   13s   polis-app/src/commands.rs
CONTENTION  same file · two workers of one agent   13s   polis-ingest/src/hook_listener.rs
CONTENTION  same file · two workers of one agent   13s   polis-ingest/src/normalize.rs
CONTENTION  same file · two workers of one agent   13s   polis-ingest/src/fswatch.rs
+28 more contention
WAITING ON YOU  you said no  12s   polis-layout/src/lots.rs
```

Thirty-three alarms is not an alarm. And every one of the 32 that outranked the real
one was **one agent's own two workers touching one file** — intra-thread, nothing to
redirect, nobody to arbitrate. PRD §11.3 defines contention as *a relation between two
threads*; this class is not that, it is the dominant population, and it buries the
primary decision under the secondary one. The ATTENTION count also swings 1 → 33 → 1
as 30-second claims expire, so the one number the operator is told to trust is the
twitchiest thing in the window.

### It is worse when the session is young

In the scratch-repo run with a real permission prompt on screen, the rail said:

```
WAITING ON YOU  permission     22s   thread c1dfc78f   nowhere on the map yet
WAITING ON YOU  notification   16s   thread c1dfc78f   nowhere on the map yet
THREADS  WAITING thread c1dfc78f   unplaced · 0 workers · 0 calls · 1 fail
```

**Zero pixels changed on the city.** The agent stops to ask *early*, before enough path
evidence exists for §6.2 convergence, so the case the product exists for is exactly the
case where the map has nothing to draw. "Nowhere on the map yet" is honest and it is
also an admission that the map did not participate in the primary decision.

Two rows for one event (`permission` + `notification`) is one decision counted twice.

---

## 3. Can you tell a failing session from a clean one without reading a number?

**Yes — this one works.** It is the best piece of visual engineering in the project.

The alarm ring (`polis-render/src/salience.rs`) is clustered, capped at eight, capped
at 9 % of the map, and built out of §11.4's actual channels: motion onset on arrival,
shape and position in steady state. It survives a 60 × 60 downsample where colour alone
did not. `salience-before-after.png` proves it against a control.

Two caveats:

* The **per-operation red outlines are not capped.** 136 failures produced roughly 40
  red-ringed glyphs across the whole city. The disciplined alarm is doing its job and
  the undisciplined outlines behind it are what turn the map into measles. The alarm is
  bounded; the thing that swamps it is not.
* Red is now so dominant that "failing" is the *only* state you can read at a glance.
  You cannot tell a failing session from a blocked one, because blocked is invisible.

---

## 4. Can you tell what an agent is doing, and how it went?

**On the map: barely. In the rail and the tree: yes, very well.**

Press `a` and the window becomes genuinely excellent:

```
WAITING ON YOU  you said no  1m 07s      polis-layout/src/lots.rs
FILE   polis-layout/src/lots.rs   [open in editor]
diff   +2929 −425   (3354 lines, Approximate)
reads / writes  16 / 45   last 1m 07s ago
verified  yes — tests ran after the last change
TOUCHED BY  Polis — city map for agents  WAITING  119× (45 writes)  last 1m 07s ago
RECENT OPERATIONS  12 of 23   … Edit done / Read done / Edit done …
```

That answers both product questions in one panel, with an outcome per operation and a
verification verdict. `t` (the tree) is better still:

```
242 files · 31 off-map · 1 attention, worst WAITING ON YOU
▼ ! polis-layout 21
    ▼ ! src 16
        blocks.rs   +2175 −257  [1] ok
      ! lots.rs     +2929 −425  [1] ok
```

**The linked filesystem view beats the city on both of the product's own decisions.**
It shows what needs you, what changed, who touched it and how it went, with no glyph
decoding and no camera. PRD §12 calls the tree "co-equal, not a fallback"; on today's
build it is simply better, and that is a finding about the map, not a compliment to the
tree.

Note also that pressing `a` **filters 33 attention items down to 1** — and the window is
dramatically more useful in that state. The UI already knows the other 32 are noise.

---

## 5. Is the map calm, or twitchy?

**Calm. Arguably frozen.** Over six screenshots 0.9 s apart on a live map with an agent
running at thousands of calls:

```
frame→frame:  0.00 %, 0.01 %, 0.02 %, 0.01 %, 0.01 %  of map-pane pixels changed by >4 L
over 4.5 s :  0.05 %
```

Nothing moves. §5's "decouple event rate from frame rate" is honoured; §13's "interpolate
everything — the entire difference between alive and steppy" is not visible at all. The
map is a still photograph that occasionally acquires a red ring. That is the right side
of the twitchy/calm trade, but it also means **motion onset — §11.4's one channel that
works in peripheral vision — is unused for anything except the alarm's 400 ms ring.**

Cost: **5.0 % of one core, 218 MB RSS** with a live agent attached. Budget is 2 % idle;
this was not idle, so no violation established, but it is not free.

---

## 6. Every element that does not change a decision

Ordered by how much of the contrast budget it wastes.

1. **Building and district labels.** Dozens of white filenames on grey plates, drawn at
   L≈120–140 over a base map clamped to L48, overlapping each other into unreadable
   soup at city zoom (see `docs/attention/window-alarm.png`: *firebaseWorkspaceService.js*
   over *ecosystemMetadataService.js* over *windowTypeRegistry.js*). They are the single
   largest consumer of the top of the range and they are layer 2. At 180 px they are
   grain. **Cut to district labels only at city tier; drop file labels to the base band.**
2. **The delegation tether fan.** Fifty-nine near-white lines converging on one hub is
   the loudest object in `docs/replay/delegation-agentolis.gif` and in every live frame I
   captured. It says "an orchestrator has workers". Nobody redirects an agent because it
   has workers. It is decoration occupying layer 4's ink.
3. **The drift mark.** `polis_world::territory::DRIFT_VERDICT` already says it: *lift
   1.14 over a 30.4 % base rate, 1.05 on the six largest sessions, "not a predictor"*.
   PRD §10.4 calls this "the redirect signal, the one thing no existing tool provides";
   it has been measured and it is not one. It is still drawn, in amber, as a large arrow
   that competes with the amber pin for the same colour channel (visible in `help.png`).
   **This is §17's own test, already run, already failed. Cut it.**
4. **Intra-thread contention.** "same file · two workers of one agent" — 32 of 33
   attention items in the live run. Not §11.3's relation-between-threads. It generates
   the aggregate illusion single-handedly.
5. **The second row for one prompt.** `permission` and `notification` fired as two
   separate `WAITING ON YOU` entries for one blocked agent.
6. **`revisited 5282×` / `revisited 93×`.** An absolute count with no denominator on a
   session of 7 510 calls. `still-thrashing-lens.png`'s 1/2/4/6-PASS trail thickness is
   the right encoding for this; the number is not.
7. **`GEN 322`, `PLACED 8930`, `RAILED 12`, `+0 -0`, `0 events, last 2s, dropped 0`** in
   the replay and live status rails. Engineering telemetry in the operator's field of
   view. `dropped 0` earns its place (§4.5 requires it); the rest does not.
8. **The duplicated header/footer.** In `window-alarm.png` the JSONL filename, event count
   and parse counters appear in full twice, top and bottom.
9. **`docs/m4-cloud-cap/` as evidence.** `fleet-03-cap-01.png`, `-05` and `-40` are
   **byte-identical** (sha256 `1f8c093f0b5e`), as are `fleet-10-cap-05` and `-40`
   (`ab2f5fe0186a`). At "fleet 40" only three cloud sites appear at all. The renders do
   not exhibit the cap and therefore do not answer §17's open question 1.
10. **The clouds themselves, on today's evidence.** I never saw an iso-contour band in a
    live run, and the 40-thread study shows three. The help sheet promises "a cloud — an
    agent, over the part of the tree it is working in"; the operator will not find one.

---

## 7. Reproducible defects

**D1 — a resolved permission never clears.** Scratch-repo run: prompt fired, rail showed
`WAITING ON YOU permission 22s`. I approved it; the command ran and failed. The alert kept
counting — 1 m 23 s, 1 m 55 s, 2 m 54 s — through the approval and into a second, different
prompt. §11.2 says "persists until resolved"; nothing resolves it. A stale unblock alarm
is worse than none: it sends the operator to a thread that is not waiting.

**D2 — the window never re-fits after a resize.** Fresh `polis watch` fits the city
correctly. `MoveWindow` to 1100 × 1150 (or to 2194 × 1234) and the city drifts into a
corner, clipped, with half the pane empty black; `r` (reset camera) does not recover it.
`Camera::set_viewport`'s doc comment describes exactly this failure and claims it fixed.
Reproduced four times, on `map` and on `watch`, growing and shrinking. An ambient
second-monitor tool that breaks when you resize it breaks on first contact.

**D3 — a blocked thread with no converged territory is drawn nowhere.** See §2. The
thread has a `cwd`, a session, and a repo root; any of those is a better answer than
"nowhere on the map yet".

**D4 — the window disappeared once** during the live watch on `C:\coding\agentolis`,
between two screenshots, with no output on stderr beyond the adapter line. Not
reproduced; noting it because a watch that silently exits is indistinguishable from a
quiet fleet. Worth a crash log.

**D5 — `0 calls · 1 fail`** on a thread that had just performed two reads and a bash
call. Hook-only threads (no transcript yet) report incoherent counters. Claude Code
v2.1.248 did not write a JSONL for the scratch session at all while it was running —
only `~/.claude/projects/<munged>/memory/` existed. If that is the new behaviour,
Channel D's "no setup needed" headline is on thinner ice than §4.4 assumes.

**D6 — 31 of 242 files are "off-map"** (tree header). 13 % of the repository has no
building. Not wrong, but the map is not the territory and nothing on the map says so.

---

## 8. What is genuinely good — do not lose it

* **`polis doctor`.** Every line that is not `ok` carries the exact command that fixes it,
  including the PowerShell one-liner for PATH. Best CLI diagnostic I have seen in a
  project this young.
* **The "waiting for an agent…" panel.** Four sources, each with its state and the reason
  it is not connected, in plain English, including "Claude Code runs no settings-file
  hooks until the workspace trust dialog has been accepted for that folder" — which is
  true, obscure, and exactly what a stuck operator needs.
* **`polis connect`.** Shows the file, the diff, the backup, the 19 events, and the two
  things it deliberately refuses to do, then asks.
* **The drill-down panel and the tree.** Section 4. This is the product working.
* **The alarm ring.** Section 3.
* **The city itself passes the squint test.** At 180 px it reads as a place — irregular
  quarters, a coastline, a dense core, tinted districts. Not a spiral, not shards, not a
  pie chart, not mould. The layout work is done and it is good.
* **`DRIFT_VERDICT`.** A load-bearing feature measured honestly and declared a failure in
  its own doc comment. That is rare and it should be acted on rather than admired.

---

## 9. The single highest-value change

**Give `needs decision` the salience treatment `failed` already has, and let it fire
even when the thread is unplaced.**

Concretely, in priority order — but the first item alone is the change:

1. **Amber gets an alarm.** Reuse `salience.rs` wholesale: expanding arrival ring
   (≤400 ms, motion onset), heavy broken steady ring with ticks, persistence that never
   fades. Size it so it produces ≥50 hot pixels in a 60 × 60 thumbnail — the same bar
   `salience.rs` set for red and cleared. Today amber produces **one**.
2. **Place it anyway.** An unplaced blocked thread pins to its `cwd` district, or to the
   civic square at repo root, with a notation that says "scope not yet known". Never
   "nowhere on the map yet" — that is the state the product is for.
3. **Delete intra-thread contention** from the attention list, and cap contention rows so
   `needs decision` is never below the fold. §11.1's ordering is right in the abstract and
   wrong in practice while contention is this noisy: an operator can act on a pending
   decision in five seconds and cannot act on two of their own subagents at all.
4. **Spend the range where §10.3 says to.** File labels and delegation tethers down into
   the base band; the top of the range reserved for layers 4–5 as written. Uncapped
   per-operation red outlines get the same clustering the alarm ring got.
5. **Cut the drift mark**, on the strength of its own measurement, and reclaim amber for
   the one state that owns it.

The test to hold this to is the project's own: take a 180 px thumbnail of a live map with
one blocked thread and forty red failures, hand it to someone across the room, and ask
them to point at where they need to go. Today they will point at the red. When they point
at the amber, the product works.

---

## 10. Two process notes

* M4 and M5 — attention, salience, contention, cloud cap, drill-down, live watch, drift —
  landed with **no ADRs**. `docs/DECISIONS.md` stops at ADR-0089 and is unmodified in the
  working tree while 28 files are not. The reasoning for the most contested decisions in
  the project is currently only in doc comments.
* `docs/replay/README.md` and `docs/GETTING-STARTED.md` are excellent and both describe a
  map that surfaces attention. On today's build the rail and the tree do that and the map
  does not. Fix the map or fix the sentence.
