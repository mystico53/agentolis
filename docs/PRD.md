# Polis — PRD

**A live, glanceable city map of what your coding agents are doing.**

`polis` is a placeholder name. Rename freely; it appears only in crate names and the binary.

---

## 0. How to read this document

This is a build spec for an implementing agent. It is opinionated on purpose — where a decision has already been made, it is stated as a decision, not an option. Sections marked **VERIFY** contain claims about Claude Code internals that drift between releases; check them against the live docs before writing code against them.

Milestones (§15) are ordered. Do not skip ahead. M1 must produce a deterministic city before any live data is wired in.

---

## 1. Summary

An operator runs dozens to hundreds of Claude Code agents across a single repository. The terminal gives them a scrolling text firehose per session and no cross-session view at all. They cannot answer three basic questions: what kind of work is happening in which threads, where is something waiting on me, and are two agents about to collide.

Polis renders the repository as an organically-grown city seen from above. Agent activity plays out on it in real time. The map is ambient — designed to be read from across the room in under a second — with drill-down for when the glance turns into a question.

**Primary user:** the operator, mid-run, with agents actively working.

**Primary decision it accelerates:** *unblock* — get to the thread that is waiting on a human.
**Secondary decision:** *redirect* — spot an agent whose scope is drifting before it finishes drifting.

Kill and review are supported but demoted to the drill-down layer.

---

## 2. Non-goals

- Not a post-hoc analytics dashboard. Cost charts, token burndowns, and usage trends belong in Grafana; there are good OTel stacks for that already.
- Not a code editor or reviewer. Clicking a building opens the file in the user's editor and stops there.
- Not multi-repo. One repository, one city. Worktrees of that repo are in scope (§7.6); unrelated repos are not.
- Not a team/multi-user product. Single operator, local machine, local data. No server, no auth, no telemetry leaving the box.
  - **"No server" means no cloud, no account, and nothing leaving the box.** It
    does not mean no local background process. `polis-sessiond` (PRD §15 M7)
    holds the ptys so that agents outlive the window; it binds `127.0.0.1`
    only, refuses to bind anything else, requires a token from a file only
    this user can read, and is never reachable from the network. See
    ADR-0098, which records why this clause is amended here rather than
    reinterpreted in a commit message.
- Not 3D. The camera is top-down orthographic with zoom and pan. No orbit, no perspective, no flying.

---

## 3. Glossary

Use these terms consistently in code and UI.

| Term | Meaning |
|---|---|
| **Session** | One Claude Code process. Has a `session_id` and a `cwd`. |
| **Main agent** | The top-level agent in a session. Orchestrates; rarely edits directly. |
| **Worker** | A subagent spawned by a main agent. Has `agent_id` and `agent_type`. |
| **Thread** | A main agent plus its worker subtree. The unit the operator thinks in. |
| **District** | A directory, rendered as a region of the city. |
| **Building** | A file. |
| **Lot** | The parcel a building sits on. Buildings are lots inset by a setback. |
| **Block** | A closed loop in the road graph. Contains lots. |
| **Street** | A cross-district import relationship, rendered as a road. |
| **Territory** | The inferred scope of a main agent — a density field, not a boundary. |
| **Cloud** | The rendered form of a territory: 2–3 discrete iso-contour bands. |
| **Attention state** | One of exactly three operator-facing alerts. See §11.2. |
| **Logical path** | Path relative to repo root, worktree-stripped. The layout key. |

---

## 4. Data ingestion

Four channels, each chosen for its cost profile. **Do not use hooks for the firehose.**

### 4.1 Channel A — OpenTelemetry (bulk events)

In-process, batched export, zero process-spawn cost. This carries the high-frequency traffic: tool calls, API requests, token counts.

Config injected into the agent environment:

```
CLAUDE_CODE_ENABLE_TELEMETRY=1
OTEL_LOGS_EXPORTER=otlp
OTEL_METRICS_EXPORTER=otlp
OTEL_EXPORTER_OTLP_PROTOCOL=grpc
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317
OTEL_LOGS_EXPORT_INTERVAL=1000
OTEL_METRIC_EXPORT_INTERVAL=10000
OTEL_LOG_TOOL_DETAILS=1
```

`OTEL_LOG_TOOL_DETAILS=1` is required — without it you get no tool names or parameters, which is most of the signal.

Polis embeds an OTLP/gRPC receiver on 4317 (`tonic` + `opentelemetry-proto`). No external collector.

**VERIFY:** exact event names, attribute keys, and the `prompt.id` correlation attribute against `code.claude.com/docs/en/monitoring-usage`. OTel support is beta and the schema moves. Parse defensively: unknown event types are logged at debug and dropped, never fatal.

### 4.2 Channel B — Hooks (rare, latency-critical events)

Hooks spawn a process per event. At ~200 tool calls/sec across a fleet, a 50ms Node or Python hook burns ten seconds of CPU per wall-clock second and lands in the agent's critical path. So hooks are used **only** for events that are rare and where latency to the display matters:

`PermissionRequest`, `Elicitation`, `ElicitationResult`, `SubagentStart`, `SubagentStop`, `TaskCreated`, `TaskCompleted`, `Stop`, `StopFailure`, `TeammateIdle`, `PostToolUseFailure`, `WorktreeCreate`, `WorktreeRemove`, `SessionStart`, `SessionEnd`, `PreCompact`, `PostCompact`.

**The hook binary** (`polis-hook`) is a static Rust binary that does exactly this and nothing else:

1. `read_to_end(stdin)` — capped at 256 KiB, truncate beyond.
2. Prepend an 8-byte header: u32 event-kind tag, u32 payload length.
3. One non-blocking `sendto()` on a Unix **datagram** socket at `$XDG_RUNTIME_DIR/polis.sock`.
4. `exit(0)` unconditionally.

Requirements:
- **Never blocks.** `SOCK_DGRAM` + `O_NONBLOCK`. On `EWOULDBLOCK`, drop the message and exit 0. Losing telemetry is acceptable; stalling an agent is not.
- **Never a FIFO.** A named pipe with no reader blocks on open and would hang every agent on the machine.
- **Never exit non-zero.** Exit 2 on a blocking event would cancel the agent's tool call. Exit 0 always, even on internal error.
- **No allocation beyond the stdin buffer.** No JSON parsing in the hook — ship the raw bytes, parse in the daemon.
- p99 wall time budget: **3ms**.

Ship a `polis install-hooks` subcommand that writes the `.claude/settings.json` hooks block and warns if an existing block would be overwritten.

### 4.3 Channel C — Filesystem watch (mutations)

`notify` crate (inotify / FSEvents / ReadDirectoryChangesW). Completely out of band; zero agent impact. Gives you the ground truth of what actually changed on disk, which OTel does not.

Tradeoff: the filesystem does not know which agent wrote. Attribute by correlating write events against the tool-call stream within a ±2s window, keyed on path. Where attribution must be certain (contention detection), fall back to the `FileChanged` hook or `PreToolUse` claims (§11.3).

Ignore: `.git/`, `node_modules/`, `target/`, `dist/`, and anything in `.gitignore`.

### 4.4 Channel D — JSONL tailing (reconciliation)

Sessions are stored at `~/.claude/projects/<munged-cwd>/<session-id>.jsonl`, where `<munged-cwd>` is the working directory with non-alphanumeric characters replaced by `-`, truncated to 200 chars with a hash suffix if longer. Files are append-only, one JSON object per line, records chained by `parentUuid` — so a session is a **tree**, not a stream.

Used for: cold-start state rebuild, replay mode (§15 M2), and filling gaps where OTel dropped events.

**VERIFY / defensive parsing mandatory.** This is undocumented internals. Model it as `{ known fields } + serde_json::Value` catch-all. A parse failure on one line must never abort the file. Fuzz this parser.

### 4.5 Event bus

All four channels normalize into one internal `Event` enum and push into a bounded `crossbeam` channel (capacity 65536) consumed by the world-state thread. On full, drop oldest and increment a dropped-events counter surfaced in the status bar. **Backpressure must never propagate to an agent.**

---

## 5. World model

Single-writer, multi-reader. One thread owns `World` and applies events; the renderer reads a lock-free snapshot (`arc-swap`) published at most once per frame.

```rust
struct World {
    repo: RepoTree,              // logical paths, sizes, git metadata
    layout: CityLayout,          // §7 output, changes rarely
    threads: HashMap<ThreadId, Thread>,
    files: HashMap<LogicalPath, FileState>,
    claims: ClaimTable,          // §11.3 contention
    attention: Vec<Attention>,   // §11.2, ordered by severity
}

struct Thread {
    id: ThreadId,
    session_id: String,
    worktree: Option<WorktreeId>,
    territory: Territory,        // §6
    workers: Vec<Worker>,
    trail: VecDeque<(LogicalPath, Instant)>,  // capped, decays
    status: ThreadStatus,        // Working | Waiting | Idle | Done
}

struct FileState {
    diff_lines: u32,             // uncommitted; drives building height
    last_touched: Instant,
    last_verified: Option<Instant>,
    touched_by: SmallVec<[ThreadId; 4]>,
}
```

**Decouple event rate from frame rate.** Never render on event arrival. Events mutate `World`; the renderer samples at its own cadence.

---

## 6. Territory inference

A main agent has no meaningful point location — it delegates rather than edits. Computing a centroid of its workers is actively wrong: an orchestrator with workers in `src/auth` and `tests/` gets a centroid in the empty gap between them, which is the one place nothing is happening.

A territory is a **density field over the layout**, estimated from sparse path observations.

### 6.1 Evidence weighting

Not all path touches carry equal signal.

**Tool-kind weights:**

| Source | Weight | Why |
|---|---|---|
| `Glob` / `Grep` with path scope | **5.0** | Declares a scope as a pattern, before any file returns. Strongest single signal. |
| `Edit` / `Write` | 3.0 | Commitment. |
| `Read` | 1.0 | Weak — could be orientation. |
| `Bash` cwd | 0.5 | Noisy. |

**Ubiquity discount (TF-IDF over your own corpus).** Every agent reads the README, `package.json`, and top-level config. Maintain a rolling count over the last N sessions of how many read each path, and scale each observation by `log(N / sessions_that_read_path)`. A path read by 90% of sessions contributes ~nothing; one read by 3% dominates. Persist this table in `$XDG_STATE_HOME/polis/corpus.db` (SQLite). Cold start with no corpus: fall back to a shipped denylist of common orientation files.

### 6.2 Convergence, not a fixed window

Do not emit a territory after a fixed number of reads. Emit when the observations **agree**:

> Let `A` = the lowest common ancestor of the last `k` weighted observations, weight-trimmed to drop the lightest 20%. Emit a territory when `depth(A) >= 2` and the trimmed weight mass inside `A` exceeds 0.7 of total.

Sometimes that is two observations, sometimes twelve. Until then the thread renders with no cloud — an unplaced marker in the status rail.

Untrimmed LCA is fragile: one stray read in `docs/` promotes the territory to repo root and claims the entire city.

### 6.3 Hysteresis

The single most important knob for whether this feels calm or twitchy.

- **Expand readily**: new mass inside or adjacent to the territory takes effect on the next update.
- **Contract slowly**: kernel weights decay with a half-life of 90s. Nothing is removed abruptly.
- **Move only on sustained evidence**: the territory's centre of mass may only shift districts after `N=8` consecutive weighted observations fall outside the current claim. One read elsewhere moves nothing.

### 6.4 Kernel density estimate

Each observation drops a Gaussian kernel at the layout position of its path. Sum the field. Everything you want falls out of this one choice:

- **Bandwidth is the uncertainty knob.** Wide and diffuse with three observations; tightens as evidence accumulates. `bandwidth = base * (1 / sqrt(effective_n))`, clamped.
- **Multi-lobed shapes come free.** An agent working in `auth` with one worker in `tests` gets two lobes and a thin connecting band — truthful, where a bounding rectangle would falsely claim the empty space between.
- **Overlap is field addition.** Two territories overlapping is just a denser region, which is exactly the contention signal.
- **Hysteresis is temporal smoothing** on the kernel weights, which you needed anyway.

---

## 7. City generation

The look comes from treating this as a growth process under constraints, not a layout algorithm. Irregularity should be the residue of history, not noise sprinkled on a grid.

### 7.1 Growth order from git

**`git log` is the growth order.** Replay it in commit order. Files added in the repo's first year form the old town — dense, tangled, irregular. Files added last month sit on the periphery and look more planned. This is not decoration: the age structure of the codebase becomes visible at a glance, and "new files bend the landscape" is simply what accretion does.

Bootstrap: `git log --diff-filter=A --name-only --reverse --format=%H|%ct`. Cache the derived growth sequence keyed on `HEAD`; recompute incrementally on new commits.

### 7.2 Pipeline — order is non-negotiable

Roads → blocks → lots → buildings. This is the Parish & Müller ordering from the CityEngine line of work. Place buildings first and connect them afterward and you get suburbia or a circuit board, every time.

**1. Terrain field.** Low-frequency simplex noise, biased by directory depth (deeper = "higher ground"). Never rendered directly. Its only job is to give roads contours to follow, so curvature looks justified rather than randomly wiggled. Cheapest source of organic-ness available.

**2. Road growth by space colonization.** Scatter attraction points weighted by where files need to be reachable. Grow segments toward unclaimed points, consuming points within a kill radius. Segments follow the terrain gradient where the slope exceeds a threshold.

> **The organic signature:** when a new segment's endpoint lands within `snap_radius` of an existing intersection, snap to it rather than creating a new node. Those irregular four- and five-way junctions are what the eye reads as "grown." Without snapping you get a tree, and trees read as artificial.

**3. Blocks** are the closed loops (faces) in the resulting planar road graph.

**4. Lots** by recursive subdivision of each block along its longest axis until lot area falls under target. Irregular blocks give irregular lots for free.

**5. Buildings** are lots inset by a setback, with a small random rotation (±4°).

### 7.3 Building attributes

- **Footprint area** ∝ `sqrt(file_size_bytes)`, clamped to `[min_lot, block_area * 0.6]`.
- **Height** ∝ uncommitted diff lines. The city rises as agents work and settles when you merge. The tallest thing on the map is the biggest unreviewed pile — which directly serves "where do I need to look."
- **Silhouette variety** carries most of the organic reading and costs nothing. Vary roof form (flat / stepped / pitched) by a hash of the path.

### 7.4 Determinism — hard requirement

**Every random draw is seeded from a hash of the logical path.** Never from wall clock, never from a global RNG, never from iteration order of a `HashMap`.

Consequences, all of which you need:
- The same repo produces the same city on every launch and on every machine. Spatial memory is the entire point of the product; a city that reshuffles is worthless.
- Growth is genuinely incremental — a new file runs one growth step, it does not regenerate the world.
- Layout becomes golden-file testable (§16).

Use `BTreeMap` wherever iteration order can affect layout. This is a common source of nondeterminism and it will be subtle when it bites.

### 7.5 Deletion and decay

Deleted files leave **vacant lots** that go to seed over time. Files untouched for a long window grow **overgrowth**. Both are free consequences of tracking `last_touched`, and together they make dead code visible without anyone running an analysis.

### 7.6 Worktrees

`git worktree` gives each agent its own directory and branch. `/repo-wt-3/src/auth.ts` and `/repo-wt-7/src/auth.ts` are the **same logical file in two physical places**.

**Key the layout on logical path relative to repo root, with the worktree prefix stripped.** Worktree and branch are a separate dimension layered over one shared base map — a filter, a tint, or a stacking offset, never a separate city. Getting this wrong means seven near-identical maps side by side and the loss of the one thing you most want to see: two agents editing the same file on different branches.

Decide this now. Retrofitting it into a layout engine later is painful.

### 7.7 Deformation rate limiting

**Never move the ground while the operator is looking at it.** Batch layout changes, apply them on a slow tween (≥800ms), and prefer to defer until the camera has been still for a moment. A map that rearranges mid-read is worse than one that is slightly stale.

---

## 8. Landmarks and wayfinding

Organic cities are harder to navigate than grids — this is true of the real ones, and it is the price of the look. The mitigation is the same one that makes Venice navigable: **landmarks carry the wayfinding, not addresses.**

Invest here. In an organic layout the monuments, district silhouettes, and skyline are how the operator orients.

| Landmark | Backing signal | Rendering |
|---|---|---|
| **Monument** | Entry points: `main`, `index`, route tables, CLI roots, files in the top decile of inbound imports | Tall, distinct silhouette, always labelled at every zoom. These are the orientation anchors — the first thing the eye finds when zoomed out. |
| **Scaffolding** | File currently under edit | Temporary-looking overlay on the building. Should read as impermanent. |
| **Overgrowth** | `last_touched` older than 90 days | Desaturated, softened outline, encroaching vegetation texture. |
| **Industrial zone** | `node_modules`, vendored, generated, `target/` | Large, uniform, deliberately dull. Making these boring is the feature — the eye should slide off them. Rendered as a single mass, not individual buildings. |
| **Civic square** | Repo root / top-level config | A recognisable open space at the historic centre. |

**District-level skeleton must stay readable at all zooms even while the street level tangles.** District boundaries, monuments, and the skyline profile are the wayfinding layer and should survive when everything else is decluttered away.

---

## 9. Streets

Streets are **cross-district import relationships only**. Intra-district coupling is expected and boring; what crosses a boundary is the architecturally interesting thing.

- Street width ∝ number of distinct import edges the street carries.
- Streets are a toggleable layer, off by default at the widest zoom.
- Diagnostics fall out for free: a district with streets to everywhere is a hub or god-module; a district with none is isolated.

Import extraction: `tree-sitter` per language, run once at index time, incrementally on file change. Failure to parse a file is non-fatal — that file simply has no streets.

**Do not let the import graph fight the directory tree for position.** The tree determines placement, because directory paths are the addressing system already in use in every tool call, error message, and conversation. Imports act only as a weak attraction force *within* a district, and as drawn edges everywhere else.

---

## 10. Visual language

Two orthogonal channels. **Shape encodes what, colour encodes how it went.** Never conflate them.

### 10.1 Operation glyphs (shape)

| Operation | Glyph |
|---|---|
| read / scan | hollow circle |
| edit | circle with a bar through |
| write | filled square |
| run (bash) | filled triangle |
| verify / test | concentric circles |
| delegate | centre dot with three satellites |

### 10.2 Outcome (colour)

Pending (neutral) · Done (teal) · Failed (red).

### 10.3 Layering and dynamic range

Five layers, bottom to top:

1. **Terrain / vacant lots** — barely visible.
2. **City** — districts, buildings, streets. Always drawn.
3. **Clouds** — territory density fields. **Rendered beneath district outlines and labels** so the map stays readable.
4. **Agents** — workers, trails, tethers.
5. **Attention** — the three states. Owns the top of the contrast range.

**Dynamic range, not omission.** Draw everything, but keep layers 1–2 inside roughly the bottom fifth of the contrast range and spend the rest on layers 4–5. Weather charts do exactly this: the coastline is always drawn and always faint; the storm system gets the ink. The operator keeps full situational awareness and the highlights still read from across the room.

### 10.4 Cloud rendering

Discrete iso-contour bands, **2–3 levels, never a continuous blur.** Continuous gradients turn to mush and you lose the ability to say "that file is in the core of this thread's work" versus "it's at the fringe."

Implementation: splat Gaussian kernels into an offscreen R16F density texture (512²), then threshold in a fragment shader to produce bands. Metaballs, essentially. A hundred kernels into a 512² target is free, and the multi-lobed shapes fall out naturally.

**Cap the number of visible clouds.** Forty threads means forty systems and the map vanishes under haze. Render clouds only for threads with an active attention state or in the top N by recent activity; let dormant territories dissipate entirely.

**Drift is the payoff.** A territory whose centre of mass is migrating out of `src/auth` toward `tests/` has a scope that is changing, and it is visible *while it is happening* — well before contention fires. Compute a drift vector from the centre of mass over the last 60s and render a leading-edge mark when its magnitude exceeds a threshold. This is the redirect signal, and it is the one thing here that no existing tool provides.

---

## 11. Attention layer

### 11.1 Ordering

`contention > needs-decision > done`. Contention is the only one where work is actively being destroyed. A pending decision costs wall clock. Done costs nothing.

### 11.2 The three states

**(a) Needs decision** — persistent, amber, drawn as a standing pin above the building or district. Sources: `PermissionRequest`, `Elicitation`, `TeammateIdle`. Persists until resolved. This is the primary state; it is what the product is for.

**(b) Done** — teal, decaying. **Main agents only.** `SubagentStop` is noise; a main thread going idle is news. Split it:
- *Done, verified* (tests ran against the changed files after the change): full weight 20s, then decays to base layer.
- *Done, unverified*: **persists.** This is really "needs review," and it is the second most important thing on the map.

Without this split, `done` floods the display within the hour and buries state (a).

**(c) Contention** — red. This is a **relation between two threads, not a property of one**, so it is drawn as a link joining them across the map, not a badge on a dot. It is also the only state that can pull the eye to two places at once.

### 11.3 Contention detection

On `PreToolUse` for `Edit`/`Write`, register a claim on the **logical path** keyed by thread, with a 30s TTL. A second live claim on the same logical path is a hit.

Severity tiers:

| Condition | Severity |
|---|---|
| Same branch, overlapping line ranges | **Critical** — work is being clobbered now |
| Same branch, same file, disjoint ranges | High |
| Different worktrees/branches, same logical file | Medium — a merge conflict you will meet later |
| One writing while another reads | Low — stale read |

**Territory overlap is the early-warning version.** Two clouds overlapping means two orchestrators are claiming the same district, and it fires *before* anyone collides — while redirecting one is still cheap. Surface it as a distinct, quieter signal than file-level contention.

### 11.4 Peripheral perception

Peripheral vision is poor at colour and good at motion onset. So:
- The **arrival** of an attention mark is a brief pulse (≤400ms). That is what catches the eye when the operator is not looking at the screen.
- Its **steady state** is shape and position, which is what gets decoded once they turn their head.
- Colour alone is never the sole channel for any state.

---

## 12. Interaction

- **Camera**: top-down orthographic. Pan (drag / arrows), zoom (scroll / +-). No rotation, no tilt.
- **Semantic zoom**, three tiers, each a genuinely different representation rather than a scale factor:
  - *City* — districts, monuments, skyline, clouds. Buildings are sub-pixel and not drawn individually.
  - *District* — buildings, streets, workers, trails.
  - *Building* — file detail panel: recent operations, which threads touched it, diff size, verification status.
- **Follow a thread**: binds the camera to a thread. **Cut, do not pan.** Agents jump discontinuously across the tree; a camera that smoothly travels from `src/auth` to `docs/` spends most of its life showing empty space.
- **Trails persist and fade** whether or not you are following, giving you history without a timeline scrubber. Trail is the natural intermediate representation between "architecture" and "diff" — you can see backtracking, thrashing (the same building revisited six times), and scope creep in it.
- **Linked filesystem view** — a plain tree, co-equal with the map, not a fallback. Shared selection and highlight state; one keystroke swaps. Both are renderings of one data structure. The map is a lossy projection and the operator will need ground truth to check it against.
- **Fuzzy above, exact below.** Soft cloud edges are honest for the ambient layer and useless when acting. Hovering a building must give a definite list of which threads touched it and when.
- **Click a building** → open in `$EDITOR` via the configured command. Nothing more.

---

## 13. Rendering architecture

**Rust + `wgpu`**, native, with `winit`. WebGPU semantics, cross-platform, and the same code compiles to WASM later if a web build is ever wanted.

- **Base map cached to a texture.** The city changes on the order of seconds; agents move continuously. Redraw the base only on layout change; composite the agent and attention layers per frame.
- **Buildings** are irregular polygons — triangulate once (`lyon`), cache in a vertex buffer, batch into a single draw call. A few thousand irregular buildings is nothing.
- **Text lives in a UI overlay, not the GPU layer.** Project world coordinates to screen and position DOM/`egui` labels. Text rendering is the classic time sink in custom renderers, and skipping it entirely gets you crisp glyphs and free styling. It also gives you label collision and decluttering, which is the one genuinely hard thing a map engine like MapLibre would have bought you.
- **Interpolate everything.** Events arrive discretely; tween agent positions, cloud density, and building heights between updates. Cheap, and it is the entire difference between "alive" and "steppy."
- **Layout runs off the render thread**, incrementally, never inside a frame.

### 13.1 Performance budgets

| Metric | Budget |
|---|---|
| `polis-hook` p99 wall time | 3 ms |
| Sustained event ingest without drop | 500 events/sec |
| Frame budget | 16.6 ms |
| Agent + attention layer draw | < 4 ms |
| Incremental layout step | < 50 ms, off-thread |
| Cold start → first frame, 5k-file repo | < 3 s |
| Idle CPU (no agent activity) | < 2% of one core |

Idle cost matters — this thing runs all day on the operator's second monitor.

---

## 14. Crate structure

```
polis/
├── polis-hook/      tiny static binary; no deps beyond libc + std
├── polis-events/    Event enum, wire format, serde defs (shared)
├── polis-ingest/    OTLP receiver, UDS listener, JSONL tailer, FS watcher
├── polis-repo/      git history, tree-sitter import graph, corpus/TF-IDF store
├── polis-layout/    terrain, road growth, blocks, lots, buildings, determinism
├── polis-world/     World state, territory inference, contention, attention
├── polis-render/    wgpu pipelines, density field, camera, culling
└── polis-app/       winit shell, UI overlay, config, CLI
```

`polis-hook` must have a near-empty dependency tree — it is spawned hundreds of times per second and its binary size and dynamic-link cost are on the critical path.

---

## 15. Milestones

Ordered. Each milestone ends in something demonstrable.

**M0 — Event spine.** `polis-events` + `polis-ingest`. OTLP receiver, UDS listener, `polis-hook`, JSONL tailer. Output: `polis tail` prints a normalized event stream to stdout. No graphics. Prove the hook meets its 3ms budget under synthetic load.

**M1 — Deterministic city, static.** `polis-repo` + `polis-layout`. Generate a city from git history and render it to a window (or PNG). No agents, no live data. **Gate: byte-identical layout across two runs and across two machines.** Do not proceed until this holds.

**M2 — Single-session replay.** Read one JSONL file offline and animate it over the city. This is the fastest possible loop for iterating on the visual language — minutes per iteration, real data, no live infrastructure. It also ships independently as a PR-summary or standup artifact. Spend real time here; most of the notation gets decided in this milestone.

**M3 — Live, single session.** Wire M0 into M2. One agent, real time.

**M4 — Multi-thread + territories + clouds.** Territory inference, KDE, iso-contour rendering, tethers to workers.

**M5 — Attention layer.** The three states, contention detection, drill-down, linked filesystem view. **This is the first milestone that delivers the actual product thesis.**

**M6 — Landmarks and polish.** Monuments, overgrowth, industrial zoning, scaffolding, trails, follow-thread camera, drift detection.

---

## 16. Testing

- **Layout determinism (golden files).** Fixture repos with pinned git history; snapshot the serialized `CityLayout`. Run on two OSes in CI. This is the most important test in the suite — everything else in the product depends on the map not moving.
- **Nondeterminism hunt.** A CI job that runs layout twice in the same process with `HashMap` iteration randomization enabled and diffs the output.
- **JSONL fuzzing.** `cargo-fuzz` against the transcript parser. Corpus seeded from real sessions. Schema drift is expected; the parser must degrade, never panic.
- **Synthetic load.** An event generator that simulates 100 threads × 400 subagents. Assert ingest budget and frame budget hold, and that dropped-event count stays at zero at 500/sec.
- **Territory inference snapshots.** Recorded real sessions with hand-labelled expected territories. Assert convergence timing (how many observations until emit) and stability (no district changes without sustained evidence).
- **Hook safety.** Assert `polis-hook` exits 0 on: no listener present, socket full, malformed stdin, oversized stdin, and `SIGPIPE`. Any non-zero exit here cancels a real agent's tool call.

---

## 17. Risks and open questions

**Risks**

- *OTel and JSONL schema drift.* Both are beta or undocumented. Mitigation: defensive parsing everywhere, hooks (documented, stable) for anything load-bearing, and a schema-drift warning in the status bar rather than a crash.
- *Fog at scale.* Enough threads and the map disappears under clouds. Mitigation in §10.4, but the cap policy will need tuning against real fleets.
- *Organic layout hurts navigability.* Accepted, deliberately. Landmarks are the mitigation and must not be treated as polish.
- *Attribution gaps from the FS watcher.* Timestamp correlation is approximate. Anything that drives an alert must come from an authoritative channel.
- *The aggregate illusion.* A beautiful swarm view that makes the operator feel informed while telling them nothing actionable is the default failure mode of this entire genre. **Test for every visual element: does it change a decision?** If not, cut it.

**Open questions**

1. What is the right cloud cap, and should dormant threads dissipate entirely or leave a faint residue?
2. Should `done, unverified` be a fourth attention state rather than a variant of `done`? It behaves more like `needs decision`.
3. Does the trail need an explicit time encoding (dash density, opacity ramp), or is fade sufficient?
4. Height from uncommitted diff means the city flattens on merge — satisfying, but does it destroy the "recently active" reading? Possibly needs a slow-decay ghost.
5. How much of the M2 replay artifact is a product on its own?
