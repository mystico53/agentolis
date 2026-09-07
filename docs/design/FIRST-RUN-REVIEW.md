# First-run review

A cold read of `docs/GETTING-STARTED.md` and `README.md`, followed literally, by
someone who had never seen Polis. No source was read to work out what a command
does; every time I wanted to, that is recorded below as a finding.

**Machine.** Windows 11 Pro 26200, 24 cores, AMD RX 9070 XT (Vulkan), German
locale, console codepage already 65001, rustc 1.98.0.

**Binary under test.** `target/release/polis.exe`, built 2026-09-02 14:40.
`cargo build --release -p polis-app` reported `Finished in 0.22s` at the start of
the review, so the binary matched the tree then. A sibling workflow moved the
tree during the review (`polis-app/src/*` went from modified to clean,
`polis-repo/src/lib.rs` and `Cargo.lock` became modified). Everything below
describes the 14:40 binary. I changed no code and committed nothing.

**Pre-existing test failure**, unrelated to anything I did and in a crate I do
not own:

```
---- the_layout_phase_scales_linearly stdout ----
POLIS_SCALING slope=1.13 (1.0 is linear) per_plot_spread=2.43x
panicked at polis-layout\tests\layout_scaling.rs:158:5:
milliseconds per plot ranged 0.412 to 1.001 (2.4x) across the four sizes
error: test failed, to rerun pass `-p polis-layout --test layout_scaling`
```

This is a wall-clock assertion and I was running Polis windows and a nested
Claude Code at the time, so it is probably load-induced rather than a real
regression. Reported, not diagnosed.

---

## The clock

Every number below is measured, not estimated.

| | |
|---|---|
| `polis` typed in a clean shell, fresh checkout | **fails immediately** — not on `PATH` |
| `cargo build --release -p polis-app` | 0.37 s **warm**. Cold is unmeasurable here; the doc claims "a few minutes" and I believe it |
| `cargo build --profile hook -p polis-hook` | 1.5 s |
| `polis watch` → window on screen | **0.08 s** |
| click a session row → city drawn and playing | **1.0 s** (app-reported cold start 1058 ms warm-cache, 2816 ms cold) |
| click → *visibly* moving (9 → 61 events, thread `idle` → `working`) | **~3 s** |
| `polis replay <file>` → city drawn and playing | **3.2 s** from process start |
| `polis snapshot` | 0.54 s; internal report says 454 ms cold start to first frame |
| `polis run -- claude -p "…"` | agent exited after 10 s; 48 events (26 telemetry, 22 transcript, 0 hooks, 0 files) |

**Seconds to first motion, honestly:** with Polis already built and on `PATH`,
**about 4 seconds of machine time** — 0.08 s to the picker, ~1 s to the city,
~3 s to the first thing that moves. That is genuinely excellent and the team
should be proud of it.

The number a newcomer actually experiences is not 4 seconds. It is 4 seconds
*plus* a cold Rust build of a wgpu/tonic stack, plus editing `PATH` by hand, plus
scanning 195 undifferentiated rows with no idea which one is a good first watch.
The engine is fast. The path to the engine is not.

**Decisions I had to make: 6.**

1. Windows-without-a-terminal (`Polis.bat`) or with one? The doc offers both and
   does not say which is better.
2. Do I build now, and do both `cargo` lines matter? (The doc says `polis run`
   does not need the second one — good — but says it 40 lines later.)
3. Where does the binary go and how do I get it on `PATH`? No command given.
4. Which of 195 sessions? No hint what makes a good first watch.
5. Click or keyboard in the picker? Nothing on screen says.
6. `polis connect` project-scope or user-scope? `GETTING-STARTED` never mentions
   the choice exists.

**Things I had to know that the docs never told me: 8.** Listed in the friction
table below as *undocumented*.

---

## Walkthrough, exactly as run

**1. Clean shell, no Polis environment.** Verified: zero `POLIS_*`, zero `OTEL_*`.

**2. `polis`** — the headline command of "Path 1 — see something move, right now".

```
polis : Die Benennung "polis" wurde nicht als Name eines Cmdlet … erkannt.
```

Not on `PATH`. Path 1 does say "and Polis itself built: … everywhere else it is
[two commands](#building-polis)", so this is documented — but the command that
cannot work yet is printed *above* the prerequisite, in the section titled
"right now".

**3. `Polis.bat`**, which the doc offers first for Windows. This works. Menu:

```
   P O L I S
   your coding agents, drawn as a city seen from above

   1. Watch a past session      replay something you already ran (195 found on this machine)
   2. Map a repository          open the city window for a checkout
   3. Connect a live agent      start Claude Code with the map watching
   4. Save a picture            write the city plan to a PNG on the Desktop
   5. Check my setup            what is wrong, and how to fix it

  In the city: every building is a file, every district a folder, and the
  tallest building is the file with the most uncommitted work - so the
  skyline points at whatever most needs reviewing.
```

That is the single best screen in the product. It states the metaphor, counts
what you already have, and defaults to option 1 on a bare Enter. It never
offered to build, because `target/release/polis.exe` already existed, so I could
not exercise the build-offer path.

**4. The documented build**, both lines, then `PATH`:

```
cargo build --release -p polis-app        # 0.37 s (warm)
cargo build --profile hook -p polis-hook  # 1.5 s → target/hook/polis-hook.exe
$env:PATH = "C:\coding\agentolis\target\release;" + $env:PATH
```

**5. `polis`** — the genuine first run. The terminal text is excellent:

```
  What you are looking at
    Every building is a file. Every district is a directory.
    A building's height is its uncommitted work, so the tallest tower
    is the biggest unreviewed pile — the skyline points at what needs
    you. …

  On this machine
    repo          C:\coding\agentolis (git checkout)
    sessions      195 past sessions, 190 replayable, 13 repositories
    Claude Code   …\claude.exe

  First run, so Polis is opening the session picker …
```

**But the window that opened alongside it was a red error**, not the picker:

> **polis could not open this**
> `no such transcript: C:\Users\konka\.claude\projects\C--coding-qurio-toolset\f2b94e93-…`
> `[pick another session]`

The transcript exists now; it did not at 14:43, while a live Claude Code was
writing it. I could not reproduce it: I parked
`%LOCALAPPDATA%\polis\first-run`, re-ran, and got the picker correctly (marker
restored afterwards). So: **on my one genuine first run, the first screen was an
error**, and it is a race against a session being written, not a deterministic
bug. It is still the worst possible first impression and the index/picker should
not hand the window an id it cannot open.

**6. `polis watch`.** Window in 0.08 s. Good picker — filter box, `195 sessions ·
190 replayable · 13 repositories`, self-labelling columns (`599 records · 96
tools · 12 files · 23 subagents · 1.4 MB`), `repository is gone` flagged in
amber. Two problems, below.

**7. Clicking a row → a city, playing.** Confirmed motion: `9 / 248 events` →
`61 / 248 events`, thread `idle` → `working`, `revisited 3×` appeared, a small
ring glyph and green triangle showed up over `coordinateTransformService.js`.
Transport bar with pause / step / `0.5× 1× 4× 16× 64×` / `next moment` /
`sessions…`, a scrubber, and `1h 43m of dead air removed`. That last line is a
lovely touch.

**8. `polis run -- claude -p "…"`** — the documented live path. Works exactly as
advertised: 48 events across two of four channels, correct exit code, stdio
passed through, and it ended by printing the exact replay command. No complaints.

---

## Friction, ranked

### Blockers

**F1 — The first command in the getting-started guide cannot work.**
`GETTING-STARTED` §"Path 1 — see something move, right now" leads with

```
polis
```

and a fresh checkout has no `polis` on `PATH`. The prerequisite is one paragraph
above with a link, and the fix is 90 lines below in §"Building Polis", where it
is one clause — *"put that folder on your `PATH` and `polis` works from
anywhere"* — with **no command for doing so on any platform**. On Windows that
is a `setx` line or a trip through the System Properties dialog, and a newcomer
who has just been told this takes "a couple of minutes" is now editing
environment variables. *Undocumented: how to actually put it on PATH.*

**F2 — Nothing in the window ever explains the map.**
The four-sentence explanation ("Every building is a file. Every district is a
directory. A building's height is its uncommitted work…") exists, is well
written, and is printed **only to stdout by bare `polis`**. It is not shown by
`polis map`, not by `polis watch`, not by `polis replay`, and not by the
double-clicked `.exe` — all of which print *zero* lines to stdout. It is not on
the `h` sheet either: that sheet has a `NOTATION` legend, but only for the agent
glyphs (read / edit / write / run / verify / delegate). **There is no legend
anywhere in the product for the three things the whole metaphor rests on.** A
person who arrives via `Polis.bat` option 2, or by double-clicking the exe, or
who scrolled their terminal, is looking at a dark polygon soup with no way to
find out what it means.

### Major

**F3 — The session picker is mouse-only, and does not say so.**
Verified with real `keybd_event` input while the window was confirmed foreground:
`Down ×3` then `Enter` does nothing at all — no selection highlight, no
navigation, no open. There is no cursor, no highlighted row, and no hint line
("↑↓ move · Enter open · type to filter"). The docs say "Pick one." A
keyboard-first user will sit there pressing Enter. *Undocumented: that you must
click.*

**F4 — The picker's sort order contradicts its own most prominent column.**
The docs promise "most recent first". The first rows read
`2026-09-02 21:27Z`, `2026-09-01 16:58Z`, `2026-09-02 20:09Z`,
`2026-09-02 21:59Z` — visibly not descending. It is in fact sorted by *end*
time (start + duration is strictly descending) while displaying *start* time.
That is defensible, but as shipped it reads as a sorting bug in the first three
seconds of looking at the product. *Undocumented.*

**F5 — Pointing Polis at a folder that is not a git repo produces a developer's
error, not a user's.**

> **polis could not open this**
> `generating the city for C:\…\notagit: reading git history: `git rev-parse --verify HEAD` failed in C:\…\notagit: fatal: not a git repository (or any of the parent directories): .git`
> `[pick another session]`

Wrapped across three full-width lines, the long path printed twice, git plumbing
exposed, and — the worst part — the only button says **"pick another session"**
when I did not ask for a session, I asked for a map. `polis doctor` promises
"Every line that is not `ok` comes with the exact command that fixes it"; this
screen has no fix. It should say: *Polis needs a git repository. Try
`polis --repo <path> map`, or `git init` here.*

**F6 — A repo with no commits silently draws a broken-looking city.**
`git init` + one untracked file → the window opens with no error at all: a
near-black rhombus, one tiny building, `1 buildings` in the status bar, no
district label, no explanation. A newcomer trying Polis on a brand-new project
concludes the product is broken. This is worse than F5, because F5 at least
tells you something is wrong.

**F7 — Windowed commands print nothing and block the terminal.**
`polis map`, `polis watch` and `polis replay` emit **zero lines** on stdout and
do not return until the window closes — I lost a 3-minute tool call to this
before I worked it out. If the window opens behind another app (which happened
repeatedly on this busy desktop) there is no feedback anywhere that anything
happened. One line — `polis: opening the map for C:\… — close the window to
return`— would fix it. *Undocumented: that these commands block.*

**F8 — `polis connect` writes the project settings file, not the user one.**
`polis connect --help` reveals a `--user` flag; `GETTING-STARTED` never mentions
that a choice exists, and says only "Claude Code's settings file". Connect once
in repo A, start `claude` in repo B, see nothing, and nothing on any screen tells
you why. *Undocumented.*

**F9 — The base map is too dark to read on first sight.**
This is PRD §10.3 working as designed (map ink clamped to channel 48, the top 80 %
of the range reserved for live layers), but the consequence is that `polis map`
and the first seconds of every replay live entirely in the dimmest 19 %. In the
captures the buildings are barely separable from the background. The product's
own thesis is "read from across the room in under a second"; the *static* case
does not meet it. I am not proposing raising the ceiling — but the static map
could use the reserved range for a first-run "here is what you are looking at"
pass, or the contrast budget could distinguish "no live layers present" from
"live layers present".

**F10 — The promised cloud never appears on a single-agent replay.**
The docs' headline visual is "A **cloud** — an agent, hovering over the part of
the tree it is working in. It leaves a fading trail behind it." Every replay I
ran reported `0 clouds (0 kernels)` in the status bar and drew one small ring
glyph. The moving thing is a few pixels wide on a 2000 px screen. What the guide
sells and what the first replay shows are not the same picture.

### Minor

**F11 — Internal jargon on user-facing surfaces.** The window title bar says
`AGENTOLIS  C:\coding\agentolis · static map (PRD §15 M1)`. The `h` sheet says
`s  streets layer (PRD §9)` and `shape is the operation, colour is how it went
(PRD §10.1, §10.2)`. `polis --help` says `replay  Animate a recorded session over
the city (PRD §15 M2)`. `polis run` warns `running without Channel A`. The
threads rail says `unplaced` in amber with no explanation. A first-time user has
no PRD and does not know what Channel A is. *Undocumented: all of it.*

**F12 — Developer telemetry is permanent chrome.** The status bar always reads
`map map district  1.0× · 21 px/building | dropped 0 drift 0` and
`labels 36+2 dropped · 154 buildings · 0 clouds (0 kernels) cold start 722 ms
frame 0.0/1.2 ms`. `map map district` reads like a bug (repeated word). None of
it means anything to a newcomer, and it is the only text on screen that never
goes away.

**F13 — `polis snapshot` buries its answer.** The useful line is
`wrote …\city.png`; it is followed by 25 lines of `solidity=0.7828`,
`p95:p05=35.1x`, `compactness=0.701`, `equalisation=80%`. `Polis.bat` option 4
("Save a picture") points a non-technical user straight at this.

**F14 — `polis doctor` exits 0 even on `FAIL`.** With 4317 occupied it printed
`1 problem that will stop something working` and returned exit code 0. And its
fix for that line is prose, not a command — it says "Stop it", but does not name
the PID it just found, and the promise at the top of the section is "the exact
command that fixes it".

**F15 — The threads rail is 35 % of the window and usually says `no threads
yet`.** On `polis map` it is dead space. On a single-agent replay it shows
`working  thread 41094a3a` — a raw id, not the session title the picker showed.

**F16 — Doc/behaviour mismatches.** `README` says the hook lands in
`target/hook/polis-hook.exe`; `polis doctor` reports the one in `target/release`.
`GETTING-STARTED` says "Click a building and the file opens in your editor. That
is the only thing clicking does" — the `h` sheet says click "select it *and* open
it in your editor". The key table in `GETTING-STARTED` lists 6 keys; the `h`
sheet lists 15 (`t`, `f`, `s`, `i`, `home`/`end`, `esc`, hover are all missing
from the guide).

**F17 — `polis run`'s map window inherits the parent's stdio handles.** After
`polis run` exits, the still-open map window keeps `stdout`/`stderr` locked, so
`polis run -- claude -p "…" > out.txt` leaves the file unreadable until you close
the window. Contradicts the promise that it "behaves in a script exactly like
`claude -p "…"`".

**F18 — `polis watch` died once with no diagnostic.** Mid-session the process
vanished; stdout empty, stderr only the adapter line, no panic, no exit message.
Not reproduced.

---

## Error messages, judged

| I did | Verdict |
|---|---|
| `polis connect` twice | **Excellent.** `Already connected — that file is exactly right. Undo with: polis connect --uninstall`, exit 0. |
| `polis connect` with no terminal | **Excellent.** `polis: this needs a yes or no and there is no terminal to ask on; re-run with --yes if you mean it`, exit 1, nothing written. |
| `polis connect --uninstall` | **Excellent.** Names the 19 registrations, explains *"Nothing else is in that file and Polis created it, so the file itself is removed — which is exactly the state before `connect`"*, and does exactly that. |
| Occupy 4317, then `polis run` | **Good.** `warning  otel: 127.0.0.1:4317 is already bound (a stale collector, or a second Polis): running without Channel A. Polis never falls back to another port, because agents are configured to export to that one.` Docked for "Channel A", and for the line directly above it still announcing `receiver 127.0.0.1:4317 (telemetry)` as if it were up. |
| Occupy 4317, then `polis doctor` | **Good.** Detects it, explains the consequence. Docked for exit 0 and for a prose fix with no PID. |
| Session id that does not exist | **Good.** Names both paths it tried and offers `[pick another session]`. Docked because nothing reaches stderr and the process never exits non-zero — unusable from a script. |
| `polis run` with a non-Claude child | **Excellent.** `Nothing arrived. That is almost always one of two things: the agent is not Claude Code … or hooks are not installed — run polis connect once.` |
| Outside a git repo | **Bad.** See F5. |
| Folder with no commits | **Bad.** See F6 — no error at all. |
| Double-click `polis.exe` | **Fine.** Window opens (2089×1324), console stays with the explainer text, nothing flashes and vanishes. The README's claim holds. |

---

## The first screen

**The picker: yes.** Title, one-line purpose (`pick a session to replay over its
city`), a filter box, honest counts, self-labelling columns, `repository is gone`
in amber. I knew what it was and what it wanted from me. Only the *how* (click,
not arrow keys) is missing.

**The map: no.** Nothing on screen says a building is a file, a district is a
directory, or that height is uncommitted work. The `h` sheet — which the guide
points at as "every key, on screen" — has a legend, but only for the agent
glyphs. The `Polis.bat` menu and bare `polis`'s stdout both explain the metaphor
beautifully, and both are surfaces the map window never shows you. So the answer
to "could you tell what to do next without asking anyone?" is: from the picker,
yes; from the map, no — you can pan and zoom a dark diagram and that is all.

---

## The single change

**Put the map legend inside the window.**

The text already exists and is already good. It is printed to a terminal that
three of the four ways into the map never show:

```
Every building is a file. Every district is a directory.
A building's height is its uncommitted work, so the tallest tower
is the biggest unreviewed pile — the skyline points at what needs you.
The old, dense core is the code you wrote first; the loose outskirts
are last month's.
```

Show it as a dismissable first-run overlay on the map (any key or click clears
it, remembered in `%LOCALAPPDATA%\polis`), and keep it permanently at the top of
the `h` sheet above the existing `NOTATION` block. That is a copy-paste of
strings that already ship, it costs one small overlay, and it converts the map
from "a pretty dark diagram" into "oh — *that* is what I am looking at" for
every entry path at once.

If there is budget for a second change, make it **F1**: move the two `cargo`
lines and a literal, copy-pasteable `PATH` command above the first `polis`
invocation in `GETTING-STARTED`, so the first command in the guide is one that
works.
