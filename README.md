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
*timestamps*, so a repository whose first year produced a twentieth of its files
still gets an old town and one imported in a single squash gets no invented
gradient at all (ADR-0064). A road is the line where one accreted parcel's
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
diverges from it — 68 recorded decisions, every one grounded in a measurement —
see [`docs/DECISIONS.md`](docs/DECISIONS.md). The evidence behind those decisions
is in [`docs/verified/`](docs/verified/).

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

Two crates are implemented; six are signatures with `todo!()` bodies and doc
comments naming the PRD section each one owes.

**`polis-events`** is the contract eight crates key on, so it is real code with
tests: `LogicalPath` and its worktree-stripping `PathMapper` (PRD §7.6 says
deciding this late is painful, and on Windows it is the most error-prone type in
the system), the hook wire codec, the event-kind tag table, and the `Event` enum
whose variant names are the real wire names from `docs/verified/`.

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
cargo test  --workspace           # 69 tests
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
| **M0** | Event spine | `polis-events` + `polis-ingest`; `polis tail` prints a normalized event stream. Prove the hook's budget under synthetic load. | **Contracts done.** `polis-events` and `polis-hook` are implemented and tested; the four channels in `polis-ingest` are signatures. |
| **M1** | Deterministic city, static | `polis-repo` + `polis-layout`; a city from git history rendered to a window or PNG. **Gate: byte-identical layout across two runs and two machines.** | **Implemented, one machine.** `polis snapshot` draws any checkout. Byte-identical across runs, processes, optimization levels, input permutation and `RandomState` order; the two-*machine* leg is wired in CI (`m1_gate.rs` leg f) and has not been observed. Measured on two real repositories — see below. |
| **M2** | Single-session replay | One JSONL file animated over the city, offline. The fastest iteration loop the project has; most of the visual notation gets decided here. | Not started. Fixtures are in `tests/fixtures/transcripts/`. |
| **M3** | Live, single session | M0 wired into M2. One agent, real time. | Not started. |
| **M4** | Multi-thread, territories, clouds | Territory inference, KDE, iso-contour rendering, tethers to workers. | Not started. Depends on the beta traces channel (ADR-0006). |
| **M5** | Attention layer | The three states, contention detection, drill-down, linked filesystem view. **The first milestone that delivers the product thesis.** | Not started. |
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
| road graph | V 915 · E 1 667 · 1 component · 0 crossings · 0 dangling | V 2 881 · E 5 226 · 1 · 0 · 0 | V 1 104 · E 1 909 · 1 · 0 · 0 |
| blocks (= independent cycles) | 753 | 2 346 | 806 |
| block area p95:p05 | 23.8× | 5.2× | 10.0× |
| age gradient (rim ÷ core) | **5.96×** | 1.65× | 2.07× |
| buildings | 3 890 / 3 890 | 7 011 / 7 014 | 4 565 / 4 565 |
| ground built on | 30.4 % (core 40.8 %) | 21.1 % | 32.2 % |
| in the road, or off their lot | 0, 0 | 0, 0 | 0, 0 |
| districts in more than one piece | 0 of 178 | 1 of 2 076 | 0 of 312 |
| full generation | **266 ms** | 400 ms | 284 ms |

against PRD §13.1's 3 s cold-start budget, and a single incremental add at 5 000
files is 37 ms median / 39 ms p95 against its 50 ms budget.

`docs/city-real-5k.png` is Neovim, `docs/city-real-django.png` is Django, and
`docs/city-real-5k-junctions.png` is the road graph alone with junctions coloured
by degree — the "is it a tree?" render, which it is not.

```sh
polis --repo ../neovim snapshot --out city.png --junctions junctions.png
polis snapshot --synthetic 5000 --out fixture.png   # the shipped fixture
```

---

## Installing the hooks

```sh
polis install-hooks            # writes the .claude/settings.json hooks block
polis install-hooks --dry-run  # show what it would write
```

Nineteen event registrations, in **exec form** — never a shell command, which
costs 6× on Git Bash and 25× on PowerShell per event (ADR-0016). `WorktreeCreate`
is deliberately **not** registered: a handler there replaces git's worktree
creation and then fails it, which would break every worktree on the machine
(ADR-0002).

In an interactive session Claude Code runs no settings-file hook until the
workspace trust dialog is accepted, so the first launch after installing can look
like a silent failure. `install-hooks` says so.

---

## What Polis is not

Not a post-hoc analytics dashboard — cost charts and token burndowns belong in
Grafana. Not a code editor or reviewer; clicking a building opens the file in
your editor and stops there. Not multi-repo: one repository, one city, worktrees
included. Not a team product: single operator, local machine, local data, no
server, no auth, nothing leaving the box. Not 3D.
