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

```sh
polis                  # the first run explains itself and opens the session picker
polis watch            # pick a session you already ran and watch it replay
polis map              # this repository, drawn as a city
polis run -- claude    # start an agent with the map already watching
polis connect          # let Polis see agents you start yourself
polis doctor           # what is wrong, and how to fix it
```

**`polis` with no arguments is the whole product for someone who has read
nothing.** It detects the checkout, Claude Code and `~/.claude/projects`,
explains the map in one screen, and — on a machine that has not run it before —
opens the session picker, because the operator already has hundreds of real
recorded sessions on disk and watching one is the shortest path from "installed"
to "I see what this is". After that, bare `polis` maps the checkout you are
standing in.

**`polis run -- claude` is the one-command connect.** It starts the OTLP
receiver, opens the map in a second process, and launches Claude Code as a child
with the twelve-variable telemetry block set **on that process** — not exported
into your shell, not written to any file. Arguments after `--` are passed
through untouched and the agent's exit code becomes the command's, so
`polis run -- claude -p "…"` behaves in a script exactly like `claude -p "…"`.
Measured on this machine against real Claude Code: 46 telemetry, 3 hook, 1
filesystem and 15 transcript events from one six-second session, all four
channels live.

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
| **M3** | Live, single session | M0 wired into M2. One agent, real time. | **Half.** `polis run -- claude` is the connect: receiver up before the agent, telemetry set on the child process only, map opened, exit code and stdio passed through faithfully, and the four channels measurably receiving. What is **not** done is the last hop — the window still reads a recording rather than the live bus, so `polis run` ends by printing the `polis replay` line for the session that just happened. Wiring the live source into the window's per-frame `advance`/`publish`/`load` is the remaining work. |
| **M4** | Multi-thread, territories, clouds | Territory inference, KDE, iso-contour rendering, tethers to workers. | Territory inference, contention and the KDE live in `polis-world`; the iso-contour clouds render in the window. Not exercised against a real multi-agent fleet, which is what would make it done. Depends on the beta traces channel (ADR-0006). |
| **M5** | Attention layer | The three states, contention detection, drill-down, linked filesystem view. **The first milestone that delivers the product thesis.** | Not started, apart from the model in `polis-world` and the linked filesystem view in the window. |
| **M6** | Landmarks and polish | Monuments, overgrowth, industrial zoning, scaffolding, trails, follow-thread camera, drift detection. | Not started. |

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
