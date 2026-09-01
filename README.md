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
last month's work sits on a more planned periphery. Roads grow by space
colonization and snap to nearby intersections, which is what makes the junctions
read as *grown* rather than generated. Buildings are files; their height is
uncommitted diff lines, so the city rises as agents work and settles when you
merge. An agent's scope is a **density field**, not a boundary — a main agent
that delegates has no meaningful point location, and a centroid of its workers
lands in the empty gap where nothing is happening. Everything is seeded from a
hash of the logical path, so the same repo produces the same city on every launch
and every machine; spatial memory is the entire point.

The spec is [`docs/PRD.md`](docs/PRD.md). Where the built system deliberately
diverges from it — 43 recorded decisions, every one grounded in a measurement —
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
| **M1** | Deterministic city, static | `polis-repo` + `polis-layout`; a city from git history rendered to a window or PNG. **Gate: byte-identical layout across two runs and two machines.** | Not started. Determinism rules and the seeded generator are specified in `polis_layout::determinism`. |
| **M2** | Single-session replay | One JSONL file animated over the city, offline. The fastest iteration loop the project has; most of the visual notation gets decided here. | Not started. Fixtures are in `tests/fixtures/transcripts/`. |
| **M3** | Live, single session | M0 wired into M2. One agent, real time. | Not started. |
| **M4** | Multi-thread, territories, clouds | Territory inference, KDE, iso-contour rendering, tethers to workers. | Not started. Depends on the beta traces channel (ADR-0006). |
| **M5** | Attention layer | The three states, contention detection, drill-down, linked filesystem view. **The first milestone that delivers the product thesis.** | Not started. |
| **M6** | Landmarks and polish | Monuments, overgrowth, industrial zoning, scaffolding, trails, follow-thread camera, drift detection. | Not started. |

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
