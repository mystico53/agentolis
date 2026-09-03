# Polis

**A live, glanceable city map of what your coding agents are doing.**

An operator running dozens to hundreds of Claude Code agents across one
repository gets a scrolling text firehose per session and no cross-session view
at all. Three basic questions go unanswered: what kind of work is happening in
which threads, where is something waiting on me, and are two agents about to
collide. Polis renders the repository as an organically-grown city seen from
above and plays agent activity out on it in real time. The map is ambient —
designed to be read from across the room in under a second — with drill-down for
when the glance turns into a question.

The city is a growth process, not a layout algorithm: `git log` supplies the
growth order, so files added in the repo's first year form a dense old town and
last month's work sits on a more planned periphery — from the commit
*timestamps*, and corrected toward the repository's own distribution only as far
as it must be, so Django (3.4 % of its files in year one) gets a legible core
while Neovim (36.3 %) is left on the calendar untouched, and a tree imported in a
single squash gets no invented gradient at all (ADR-0064, ADR-0073). A road is the line where one accreted parcel's
territory stops and the next one's begins, which makes planarity, connectivity
and closed blocks properties of the construction rather than of a tuning constant
(ADR-0052), and the four- and five-way junctions that read as *grown* fall out of
it. Buildings are files; their height is
uncommitted diff lines, so the city rises as agents work and settles when you
merge. An agent's scope is a **density field**, not a boundary — a main agent
that delegates has no meaningful point location, and a centroid of its workers
lands in the empty gap where nothing is happening. Everything is seeded from a
hash of the logical path, so the same repo produces the same city on every launch
and every machine; spatial memory is the entire point.

The spec is [`docs/PRD.md`](docs/PRD.md). Where the built system deliberately
diverges from it — 72 recorded decisions, every one grounded in a measurement —
see [`docs/DECISIONS.md`](docs/DECISIONS.md). The evidence behind those decisions
is in [`docs/verified/`](docs/verified/).

---

## Start here

If you have never run this before, read
[`docs/GETTING-STARTED.md`](docs/GETTING-STARTED.md) instead of this file. It is
two pages and assumes nothing.

From a fresh clone, two commands — the first takes a few minutes, once:

```sh
cargo build --release -p polis-app
./target/release/polis                  # .\target\release\polis.exe on Windows
```

That second line is the whole product for someone who has read nothing. Once the
binary is [on your `PATH`](#putting-polis-on-your-path) — `polis doctor` prints
the exact command for this machine, and `Polis.bat` offers to do it for you — it
is just `polis`:

```sh
polis watch            # every agent working in this repository, live
polis watch --list     # the same answer as text, opening nothing
polis                  # the first run explains itself
polis map              # this repository, drawn as a city
polis run -- claude    # start an agent with the map already watching
polis replay           # pick a session you already ran and watch it back
polis connect          # add hook detail to what a watch already sees
polis doctor           # what is wrong, and how to fix it
```

The three that open a window — `watch`, `map`, `replay` — say on stdout what they
are opening and do not return until you close it.

**`polis watch` is the headline command, and it needs nothing set up.** Point it
at a repository and it shows every Claude Code session working in it right now —
**including the ones you started yourself, in other terminals, that Polis never
launched**. Every session on this machine already writes a JSONL transcript
carrying the directory it is working in, so presence, activity and every tool
call are on disk before Polis is involved at all. Sessions that start after the
window opens are picked up as they appear; sessions in other repositories are
listed and labelled rather than silently drawn onto the wrong city. Telemetry
(`polis run`) and hooks (`polis connect`) then add detail on top — token counts,
subagent attribution, sub-second latency, and `Stop`, which is the only signal
that proves a session ended rather than merely going quiet. The window and
`polis doctor` both say which of the three are connected, so an operator seeing
less than they expected can find out why at a glance.

**`polis` with no arguments is the whole product for someone who has read
nothing.** It detects the checkout, Claude Code and `~/.claude/projects`,
explains the map in one screen, and — on a machine that has not run it before —
opens the session picker, because the operator already has hundreds of real
recorded sessions on disk and watching one is the shortest path from "installed"
to "I see what this is". After that, bare `polis` maps the checkout you are
standing in. To see what is happening *now* rather than what happened, that is
`polis watch`.

**`polis run -- claude` is `polis watch` with telemetry added.** It opens a
`polis watch` window — which owns the receivers, so there is one ingest stack and
it is in the process that draws — and launches Claude Code as a child with the
twelve-variable telemetry block set **on that process**, not exported into your
shell and not written to any file. Arguments after `--` are passed through
untouched and the agent's exit code becomes the command's, so
`polis run -- claude -p "…"` behaves in a script exactly like `claude -p "…"`.
You do not need it to see an agent: `polis watch` already shows the one you
started yourself. Measured on this machine against real Claude Code: 46
telemetry, 3 hook, 1 filesystem and 15 transcript events from one six-second
session, all four channels live.

On Windows, `Polis.bat` is double-clickable and offers the same choices with no
terminal at all — including offering to build Polis the first time. Double-
clicking `polis.exe` itself is also safe now: with no arguments it opens a
window, and on the one path where that can fail it prints why and waits for a
keypress instead of closing the console before it can be read.

---

## Crate map

```
polis-hook/      the hook transport binary. std only, zero dependencies.
polis-events/    the shared contract: Event, the wire format, LogicalPath, ids.
polis-ingest/    OTLP receiver, hook listener, JSONL tailer, filesystem watcher.
polis-repo/      git growth order, tree-sitter import graph, TF-IDF corpus store.
polis-layout/    terrain, roads, blocks, lots, buildings, determinism.
polis-world/     World state, territory inference, contention, attention.
polis-render/    wgpu pipelines, density field, camera, culling.
polis-app/       eframe/winit shell, UI overlay, config, the `polis` binary.
```

Dependencies flow one way:

```
polis-events ──┬── polis-ingest ─────────────────────────┐
               ├── polis-repo ── polis-layout ── polis-world ── polis-render ── polis-app
               └───────────────────────────────────────────────────────────────┘

polis-hook     (depends on nothing at all, not even polis-events)
```

Seven of the eight crates are implemented and tested. The remaining stubs are
`polis-render`'s **wgpu** pipeline modules (`agents`, `camera`, `city`,
`density`, `marks`) — 22 `todo!()` bodies naming the PRD section each one owes.
The window does not depend on them: it draws the base map through
`polis-render`'s deterministic CPU rasteriser and composites the live layers in
`egui`, which is what PRD §13 asks for anyway (*"text lives in a UI overlay, not
the GPU layer"*). The GPU path is the optimisation, not the product.

**`polis-events`** is the contract eight crates key on: `LogicalPath` and its
worktree-stripping `PathMapper` (PRD §7.6 says deciding this late is painful, and
on Windows it is the most error-prone type in the system), the hook wire codec,
the event-kind tag table, and the `Event` enum whose variant names are the real
wire names from `docs/verified/`.

**`polis-hook`** is the verified reference implementation from
[`docs/verified/hook-ipc.md`](docs/verified/hook-ipc.md), which passed a 36-row
exit-code safety matrix and full latency characterisation. It is on a real
agent's critical path, so it is small, proven, and dependency-free.

---

## Building

Requires **Rust 1.98.0**, pinned in `rust-toolchain.toml`. Not the machine
default: the egui 0.36 family declares `rust-version = 1.95` and `cargo check`
refuses outright on 1.94.0, with a message that reads like a dependency problem
rather than a toolchain one. See ADR-0001.

```sh
cargo build --workspace           # everything
cargo test  --workspace           # 973 tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

The hook binary has its **own profile**, because size and dynamic-link cost are
on the critical path of every hook event on the machine:

```sh
cargo build --profile hook -p polis-hook      # → target/hook/polis-hook.exe
cargo tree  -p polis-hook                     # must print exactly one line
```

`cargo tree -p polis-hook` printing anything but the package itself is a
regression, and CI fails on it.

The first build compiles wgpu and tonic and takes a few minutes.

### Putting Polis on your `PATH`

`cargo build --release -p polis-app` lands the binary in `target/release/polis`
(`target\release\polis.exe` on Windows). It runs from there as it stands; putting
that folder on `PATH` is what makes the command `polis`.

```powershell
# Windows PowerShell — your user PATH, not the machine's. New terminals see it.
[Environment]::SetEnvironmentVariable('Path',
  [Environment]::GetEnvironmentVariable('Path','User') + ';C:\path\to\agentolis\target\release', 'User')
```

Not `setx PATH "%PATH%;…"`: `%PATH%` there is the combined machine **and** user
value, so that line copies the whole system path into your user one, permanently,
truncated at 1024 characters.

```sh
# macOS / Linux — this shell, and then your shell profile to keep it.
export PATH="$PWD/target/release:$PATH"
```

`polis doctor` prints whichever of these applies with this machine's own path
already substituted in, and on Windows `Polis.bat` offers to do it for you the
first time it runs.

### House style

Stub bodies are `todo!("PRD §X — what is owed")` with a `let _ = (params);` line
above so the signature keeps meaningful parameter names rather than
underscore-prefixed ones. By-value parameters a stub cannot yet consume are
`drop(param)`, which is honest about ownership. There are no blanket `allow`
attributes; the two lint carve-outs live in `clippy.toml` with reasons.

---

## Milestones

Ordered. Each ends in something demonstrable. Do not skip ahead — PRD §15.

| | Milestone | Delivers | Status |
|---|---|---|---|
| **M0** | Event spine | `polis-events` + `polis-ingest`; `polis tail` prints a normalized event stream. Prove the hook's budget under synthetic load. | **Done.** All four channels run; `polis tail` prints the normalized stream. `polis-hook` passed a 36-row exit-code safety matrix and is registered in exec form (ADR-0016). Observed live against real Claude Code through `polis run`: telemetry, hook, filesystem and transcript events all arriving in one session. |
| **M1** | Deterministic city, static | `polis-repo` + `polis-layout`; a city from git history rendered to a window or PNG. **Gate: byte-identical layout across two runs and two machines.** | **Done on one machine; the two-machine leg is unobserved.** `polis map` opens the window and `polis snapshot` writes the PNG. Byte-identical across runs, processes, optimization levels, input permutation and `RandomState` order; the two-*machine* leg is wired in CI (`m1_gate.rs` leg f) and has not been observed. Both PRD §13.1 budgets hold on both real repositories — see below. The geometry is a partition of the plot adjacency graph rather than of the plane (ADR-0074): solidity 0.995-0.999 -> **0.76-0.83** on four corpora, radial spokes 4-9 -> **0**, boulevards through the civic square 1 -> **0**, and the longest dead-straight district border 46-93 % of the diameter -> **6.7-11.1 %**. All three are asserted by the gate rather than measured in a notebook. |
| **M2** | Single-session replay | One JSONL file animated over the city, offline. The fastest iteration loop the project has; most of the visual notation gets decided here. | **Done.** `polis watch` picks from this machine's own recordings — 187 sessions indexed in 67 ms headers-only — and animates the chosen one over its repository's city with a two-timeline transport clock. 22 759 real events applied in 239 ms (10.5 µs/event, ~190× PRD §13.1's 500/s budget); a 26.1-hour session compresses to 18.4 watchable minutes. |
| **M3** | Live, single session | M0 wired into M2. One agent, real time. | **Done.** The window drives the world from the live bus, once per frame. `polis watch` needs **no setup at all**: three real `claude` sessions started by hand in one checkout, with no hooks and no `OTEL_*` exported, all appeared — verified end to end, see below. Hooks and telemetry are enrichment on top, and the status rail says which of the four channels are actually delivering, because the failure this milestone existed to fix was silence. |
| **M4** | Multi-thread, territories, clouds | Territory inference, KDE, iso-contour rendering, tethers to workers. | **Done, exercised against a real fleet.** Three real agents in one checkout became three threads, never merged; worker attribution 1/1 via `record agentId`; territory converged in 2-3 observations (5.0-11.5 s) onto exactly the directory each agent was told to work in, with zero district changes. Clouds render live: **5 clouds / 15 kernels** on a concentrated fleet. A thread whose reads are spread across unrelated directories correctly gets **no** cloud — PRD §6.2's convergence gates refusing to claim, not a missing feature. |
| **M5** | Attention layer | The three states, contention detection, drill-down, linked filesystem view. **The first milestone that delivers the product thesis.** | **Done, with one live defect (below).** All three states observed firing live, which no replay can do: contention (`same file · two agents`), *needs review* (`done, unverified`), and `done · tests ran after the change`. Contention is now keyed by **actor** `(thread, worker)`, not by thread — **4 of 4 real contentions in the operator's corpus are worker-versus-worker inside one session**, the exact class the old thread-keyed table could not represent. Failure salience: the reddest real frame went from 3 hot px / 3600 to **42** (0.08 % -> 1.17 %), plus a ≤400 ms arrival pulse. Drill-down and the linked tree view are in. |
| **M6** | Landmarks and polish | Monuments, overgrowth, industrial zoning, scaffolding, trails, follow-thread camera, drift detection. | **Mostly built incidentally; not yet a milestone.** Monuments (`MAX_MONUMENTS = 24`; 4-24 found on every real repo tried), overgrowth (`overgrowth_over`, 90 -> 270 days), industrial zoning (fires on real repos: 5 districts in `stickingplacebooks`, 1 in `biwt`), scaffolding (`MAX_SCAFFOLDS = 24`), trails, `FollowCamera` (bound to `f`, cut not pan) and drift (`DRIFT_CONFIRMATIONS = 8`, shown in the rail) all exist and are tested. What M6 still owes is the pass that decides which of them **change a decision** (PRD §17) and cuts the rest. |

### M1, measured

Generation is a growth simulation: the Voronoi diagram of parcels accreted one
file at a time in `git log` order, inside a territory partition that gives every
directory its own polygon (ADR-0052, ADR-0056). PRD §7.1's age gradient is driven
by **real commit timestamps**, not by position in the file list (ADR-0064), so a
repository's age structure is a property of its history rather than of the
generator.

Two real open-source repositories, cloned with full history, and the shipped
5 000-file fixture — one build, no per-corpus tuning:

| | Neovim | Django | fixture |
|---|---|---|---|
| files / history | 3 890 · 12.6 y | 7 014 · 21.1 y | 4 965 · 8.0 y |
| added in year one | 36.3 % | 3.4 % | 8.2 % |
| age-ramp correction | **0 %** — the calendar is already right | 47 % | 45 % |
| files in the core band | 36.3 % | **26.2 %** (was 3.4 %) | 25.3 % |
| road graph | V 912 · E 1 666 · 1 component · 0 crossings · 0 dangling | V 2 865 · E 5 270 · 1 · 0 · 0 | V 1 161 · E 2 015 · 1 · 0 · 0 |
| blocks (= independent cycles) | 755 | 2 406 | 855 |
| 4- and 5-way junction share | 57.1 % | 50.3 % | 45.2 % |
| block area p95:p05 | 22.8× | 7.1× | 13.6× |
| age gradient (rim ÷ core) | **6.12×** | **2.33×** (was 1.65×) | 2.96× |
| buildings | 3 890 / 3 890 | 7 011 / 7 014 | 4 565 / 4 565 |
| ground built on | 29.2 % (core 48.1 %) | 21.4 % | 32.8 % |
| distinct building heights | 1 998 | 2 429 | 2 036 |
| in the road, or off their lot | 0, 0 | 0, 0 | 0, 0 |
| districts in more than one piece | 0 of 178 | 0 of 2 076 | 0 of 312 |
| full generation | 282 ms | 499 ms | 295 ms |

### The two PRD §13.1 budgets

**Cold start to first frame, under 3 s.** Both halves of the derived history now
come out of **one** `git log --name-status` pass instead of two, and the result
is cached keyed on `HEAD` exactly as PRD §7.1 asks (ADR-0069, ADR-0070). Wall
clock, `polis snapshot` end to end, one machine:

| | Neovim | Django |
|---|---|---|
| before | 2.77 s | **3.13 s** |
| first launch ever, nothing cached | 2.58 s | 4.43 s (a 35 k-commit, 21-year history) |
| every launch after | **0.99 s** | **1.86 s** |

**Incremental layout step, under 50 ms off-thread.** A single add at 5 000 files
moves a median of four of 1 165 road nodes, and the step now reuses 87 % of the
city's buildings and 71 % of its block cuts instead of recomputing them
(ADR-0071). Release, twelve adds:

| | median | p95 |
|---|---|---|
| before, 24 threads | 44.1 ms | 46.5 ms |
| before, 1 thread | 61.2 ms | 71.9 ms |
| **after, 24 threads** | **37.7 ms** | **43.3 ms** |
| **after, 1 thread** | **36.0 ms** | 44.7 ms |
| after, machine half-loaded | 39.0–45.5 ms | 47.7–61.8 ms |
| after, machine fully saturated | 68–78 ms | 155–172 ms |

The interesting row is not the fastest one: it is that 24 threads and 1 thread
now give the same answer, because the work was **removed** rather than spread
across cores that a CI runner does not have. No algorithm meets a wall-clock
budget on a fully saturated machine, and the last row is reported rather than
asserted.

Renders are regenerated from the real corpora, never from the synthetic
fixture: a fixture can be accidentally flattering and these two are not.
`docs/city-real-5k.png` is Neovim, `docs/city-real-django.png` is Django, and
`docs/city-real-5k-junctions.png` is the road graph alone with junctions coloured
by degree — the "is it a tree?" render, which it is not.

The same applies to the **gate**, and it did not until this round. The M1
acceptance table now runs against three real repositories checked into
`tests/corpora/` as manifests — `click` (166 files, 12 years), `pytest` (690
files, 18 years) and a pinned capture of this repository on its first day — and
not only against `polis_repo::synthetic`. See `tests/corpora/README.md` and
ADR-0080; the defect that found is ADR-0078.

```sh
polis --repo ../neovim snapshot --out city.png --junctions junctions.png
polis snapshot --synthetic 5000 --out fixture.png   # the shipped fixture
```

### M3-M5, measured end to end

Everything below was measured on one machine (Windows 11, 24 cores, rustc
1.98.0) against **real Claude Code sessions**, not fixtures.

**The operator's sentence, tested literally.** *"i want to see all agents active
in a repository on the machine."* Three `claude` processes started in one
checkout from separate shells, with no hooks registered and no `OTEL_*` in the
environment. All three appeared, correctly scoped, with sessions in other
repositories listed but **not drawn** onto the wrong city. Ten at once: all ten
finished correctly, 0 dropped events, 0.4/0.5 ms frames, no panic.

| PRD §13.1 budget | Target | Measured | |
|---|---|---|---|
| Frame | 16.6 ms | **0.4-4.8 ms** live, worst observed | pass |
| Agent + attention draw | < 4 ms | **p50 1.2 µs, p95 28.6 µs** over a real 37 437-event session; **1.60 ms** with 48 marks in all four states, all escalated | pass |
| Incremental layout step | < 50 ms | median **42.4 ms**, p95 **45.8 ms** | pass, thin |
| Sustained ingest, zero drops | 500 ev/s | 500/s x 25 s: **12 499 sent, 12 499 received, 0 dropped**. Headroom: 5 000/s x 12 s, **59 995 / 59 995, 0 dropped** | pass, 10x |
| Idle CPU | < 2 % of one core | **1.56 %** over 45 s (54 threads, 156 MB RSS) | pass, thin |
| Cold start -> first frame, 5 k files | < 3 s | warm **816 ms**; cold **3.9-7.6 s** when all 5 000 files are parseable source | **miss, 1.3-2.5x** |

**The cold-start miss is real, reproducible, and narrower than it looks.** It is
not the layout and not the render — it is tree-sitter, once, on first sight of a
repository. Five thousand files in one language, nothing cached, this machine:

| all 5 040 files are | imports | cold start | warm |
|---|---:|---:|---:|
| `.py` | 2 896 ms | **3 923 ms** | — |
| `.ts` | 3 945 ms | **4 936 ms** | — |
| `.rs` | 4 915 ms | **5 933 ms** | 816 ms |
| `.rs`, no `use` statements at all | 5 161 ms | **6 156 ms** | — |
| `.js` | 6 557 ms | **7 595 ms** | — |

Removing every import statement did not help, so this is per-file parse cost, not
edge resolution. It is **0.86-1.30 ms per parseable file** here, and the same
0.86 ms/file falls out of `qurio-toolset` (1 557 files, imports 1 340 ms cold).

That number is what makes **ADR-0082's Django row unrepresentative**: it reports
423 ms of imports over 7 014 files, or 0.06 ms/file — fourteen times cheaper than
any measurement in this round, across four grammars and a real repository.
Django's tree is mostly `.html`, `.po`, migrations and static assets, so most of
those 7 014 files were never handed to a grammar. The budget therefore holds for
a repository with a typical mix, and misses for one whose 5 000 files are *all*
source — a large Rust or TypeScript monorepo, which is not an exotic case. Every
launch after the first is 816 ms, because the import cache is keyed on content
rather than path. `polis snapshot` prints the budget beside the measurement and
does not warn when it is over.

**M4 against a real fleet.** Three agents, one checkout, watch started before
anything was running in it:

```text
threads              3 now, 3 at peak          workers 1/1 attributed (100.0%) via record agentId
converged after 2 observations (11.5s)  claim src/notes     depth 2 mass 1.00
converged after 3 observations (5.1s)   claim src/auth      depth 2 mass 1.00
converged after 3 observations (5.5s)   claim src/render    depth 2 mass 1.00
contention 0        health: drift 0, unmapped 0, retired 0
```

Each thread claimed exactly the directory its prompt named, and no claim ever
changed district. Clouds render at **5 clouds / 15 kernels**.

**M5's three states, live.** A replay structurally cannot show contention (it
needs two threads) or *done* (it needs `Stop`). Live does:

```text
CONTENTION    same file · two agents   23s    thread aaaa1111  src/mod1.rs
needs review  no test ran after the change  20s   thread bbbb1111  src/mod1.rs
done          tests ran after the change   14s   thread 574d4485  src/mod1.rs
```

The status rail's worst-first summary flips from `WAITING ON YOU` to
`CONTENTION` when one fires, which is PRD §11.1's ordering doing its job.

**Contention is keyed by actor, and that is what made it fire.** On the
operator's own corpus: **4 of 4 real contentions are worker-versus-worker inside
a single session** — `settings.css` (two subagents 4.7 s apart), plus
`polis-render/src/live.rs`, `polis-world/src/apply.rs` and `polis-app/src/cli.rs`
in a 69-worker fan-out. A `ThreadId`-keyed table represents **none** of them.
Caveat: `edits_without_line_ranges` is 1 768 against 8 hits, so PRD §11.3's
**Critical** tier (overlapping line ranges) is effectively unreachable from the
transcript and everything lands at **High**.

### The live defect this verification found

**A headless `claude -p` session that has exited still reads `WAITING ON YOU`,
for ever.** `DecisionSource::TurnEnded` fires when a main agent ends its turn
with no tool call and no human reply yet. For an interactive session that is
exactly right and is the reason a replay can show the primary state at all. For
a headless one, the process is *gone* and no human will ever reply, so the mark
is permanent and false; `retire_threads` then deliberately never retires a thread
an attention mark still points at, so the pins accumulate. Observed: 12 sessions
run, 12 threads on the map, 12 amber pins, none of them real, ageing past 3
minutes and climbing.

It is fixable and the evidence is already on disk. Every transcript record
carries `entrypoint`, which `docs/verified/jsonl-schema.md` records as **STABLE
100 %** with values `"cli"` (125 553) and `"sdk-cli"` (120), and which
`polis-ingest` already parses into `TranscriptRecord::entrypoint`. **Nothing in
`polis-world` reads it.** Gating `TurnEnded` on an interactive entrypoint is the
fix. Impact is small for an operator typing at a terminal (`sdk-cli` is 0.1 % of
the real corpus) and total for anyone driving a fleet with `claude -p` — which is
the shape PRD §16's synthetic load assumes.

---

## Installing the hooks

```sh
polis connect              # shows the file, the diff and the backup, then asks
polis connect --dry-run    # everything, plus the full file it would write
polis connect --uninstall  # removes what it added and puts the file back
```

`polis connect` is `install-hooks` with consent: it names the exact file, prints
the nineteen registrations, shows a line diff against what is there now, states
that the current file is copied to `<name>.polis-backup` first, and then waits
for a yes. With no terminal to ask on it refuses rather than assuming, and
`--yes` is the explicit override for scripts. `polis install-hooks` is still
there and unchanged for the non-interactive case.

Nineteen event registrations, in **exec form** — never a shell command, which
costs 6× on Git Bash and 25× on PowerShell per event (ADR-0016). Two rules are
enforced twice, once in `polis-ingest` where the block is built and once in
`polis_app::setup::audit` against the JSON about to be written:

* `WorktreeCreate` is **never** registered. A handler there replaces git's
  worktree creation and then fails it, which would break every `git worktree`,
  every `claude --worktree` and every isolated subagent on the machine
  (ADR-0002, `hooks-schema.md` §9.1).
* `PreToolUse` is **narrowed and anchored** to `^(Edit|Write|NotebookEdit)$`.
  Unmatched it is the ~200 call/sec firehose PRD §4.2 exists to avoid, and
  unanchored `Edit` also matches `NotebookEdit` (`hooks-schema.md` §9.3).

The uninstall is tested rather than asserted: install into a settings file that
already had other keys and other tools' hooks in it, uninstall, and the parsed
JSON has to equal what was there before — and when Polis created the file, "put
it back" means the file is gone.

In an interactive session Claude Code runs no settings-file hook until the
workspace trust dialog is accepted, so the first launch after installing can look
like a silent failure. `connect` says so, on the screen, right after it writes.

---

## What Polis is not

Not a post-hoc analytics dashboard — cost charts and token burndowns belong in
Grafana. Not a code editor or reviewer; clicking a building opens the file in
your editor and stops there. Not multi-repo: one repository, one city, worktrees
included. Not a team product: single operator, local machine, local data, no
server, no auth, nothing leaving the box. Not 3D.
