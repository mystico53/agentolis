# Polis — Architecture Decision Record

Where the built system deliberately diverges from `docs/PRD.md`, and why.

Every entry below is grounded in a compile-proven or measured finding from
`docs/verified/`. The PRD is the spec; this file is the errata plus the design
calls the spec left open. When the two disagree, **this file wins**, and the PRD
should be amended to match.

Provenance for each finding is one of six recon documents, all in
`docs/verified/`:

| File | Covers |
|---|---|
| `hooks-schema.md` | every hook event, payload, exit-code contract, `settings.json` format |
| `otel-schema.md` | OpenTelemetry event/metric inventory, env vars, the beta trace span tree |
| `otlp-receiver.md` | the tonic/opentelemetry-proto receiver, backpressure, wire types |
| `jsonl-schema.md` | transcript format over 866 files / ~153 613 records |
| `hook-ipc.md` | hook transport: 36-row safety matrix, full latency distributions |
| `gpu-stack.md` | eframe/wgpu/lyon stack, verified on real hardware |

Unless stated otherwise, every measurement was taken on Windows 11,
`x86_64-pc-windows-msvc`, against Claude Code **2.1.248**. Assume drift; gate
observations on `service.version` and the logs scope version.

---

## ADR-0001 — Pin the toolchain to 1.98.0; the workspace floor is 1.95, not 1.94

**Context.** The environment brief specified `rust-version = "1.94"` and a
`rust-toolchain.toml` pinning 1.94.0, which is the machine's default `stable`.
`gpu-stack.md` reported that the egui 0.36 family requires 1.95. This workspace
re-verified it directly:

```
error: rustc 1.94.0 is not supported by the following packages:
  ecolor@0.36.1 requires rustc 1.95
  eframe@0.36.1 requires rustc 1.95
  egui@0.36.1 requires rustc 1.95
  … egui-wgpu, egui-winit, emath, epaint, epaint_default_fonts
```

The failure happens at *resolution* time and reads like a dependency problem
rather than a toolchain one, which is exactly the trap a fresh clone falls into.

**Decision.** `rust-toolchain.toml` pins **1.98.0** (already installed on this
machine, and the toolchain the GPU probe compiled against).
`[workspace.package] rust-version = "1.95"` records the real floor.

**Consequences.** The brief's 1.94 instruction is knowingly overridden; there is
no version of this workspace that both contains eframe 0.36.1 and builds on
1.94.0. `polis-hook` is std-only and would build far below 1.95, but shares the
workspace lockfile and therefore the pin. CI installs the pinned toolchain
rather than `stable`, so a new Rust release cannot break the build overnight.

---

## ADR-0002 — Never register a `WorktreeCreate` hook

**Context.** PRD §4.2 lists `WorktreeCreate` among the events `polis-hook`
registers. The hooks reference states that configuring a `WorktreeCreate` hook
**replaces git's default worktree behaviour**, that the hook must return the path
of the worktree it created, and that any non-zero exit — or no path — fails
worktree creation. `polis-hook` prints nothing and exits 0.

**Decision.** `WorktreeCreate` has no tag in `polis_events::EventKind`, is absent
from the `install-hooks` template, and is asserted absent by
`kind::tests::worktree_create_has_no_tag`. Worktree births are observed through
`SessionStart` + `cwd`, `CwdChanged`, `git worktree list`, or the filesystem
watcher. `WorktreeRemove` is safe and stays.

**Consequences.** Registering it would have suppressed `git worktree` entirely
and then failed creation, breaking every `claude --worktree` session, every
`isolation: "worktree"` subagent and every background session **on the machine** —
while PRD §7.6 makes worktrees a first-class feature. Remove `WorktreeCreate`
from PRD §4.2's list.

---

## ADR-0003 — `FileChanged` is not the attribution fallback

**Context.** PRD §4.3: *"Where attribution must be certain (contention
detection), fall back to the `FileChanged` hook or `PreToolUse` claims."* Two
facts kill that use. The hook watches a **literal, explicitly named filename
list** — the matcher value is split on `|` and each segment registered as a
literal filename; regex is not useful there — and watching a repository would
require either enumerating every file into a matcher or returning `watchPaths` as
JSON on stdout, which PRD §4.2 forbids the hook from producing. Worse, its
payload is `{ file_path, event }` plus common fields: **no `tool_name`, no
`tool_use_id`**, and it fires no matter what changed the file, including a
process outside Claude Code entirely.

**Decision.** Delete "the `FileChanged` hook or" from PRD §4.3. It is not
registered. `PreToolUse` claims are the only authoritative attribution channel.

**Consequences.** It is exactly as attribution-blind as the `notify` watcher it
was meant to backstop, while additionally costing a process spawn per event.

---

## ADR-0004 — `PreToolUse` is mandatory for contention; the line-range tier degrades for subagents

**Context.** PRD §4.2's hook list omits `PreToolUse` while PRD §11.3 opens *"On
`PreToolUse` for `Edit`/`Write`, register a claim"* — an internal contradiction.
Separately, PRD §11.3 tiers contention severity by overlapping line ranges, and
`structuredPatch` inside `toolUseResult` is the only source of line ranges.
Measured: `toolUseResult` is present on **15 542 of 15 542** main-thread tool
results and **absent on 14 811 of 22 697 subagent tool results (65%)**, including
625 `Edit` calls. Whole subagent files carry zero sidecars.

**Decision.** Register `PreToolUse`, matcher-narrowed to
`^(Edit|Write|NotebookEdit)$`, and promote it from fallback to the **primary**
contention channel. `polis_world::contention::classify` degrades to at most
`Severity::High` ("same file") whenever either claim lacks a line range —
always the case for a subagent edit — rather than guessing at overlap.

**Consequences.** The matcher is what keeps `PreToolUse` from being the
~200 calls/sec firehose §4.2 exists to avoid; an unmatched registration would be
precisely that. Anchoring is required because unanchored `Edit` also matches
`NotebookEdit`. Anything reading paths, diff counts or exit status from
`toolUseResult` will work perfectly in single-threaded testing and silently lose
most of its data the moment subagents are involved.

---

## ADR-0005 — Never persist PII, tool payloads, or raw API bodies

**Context.** The default OTel stream is not clean. `user.email` — a real address —
rides on **every event and every metric datapoint**, with no environment variable
that suppresses it. `organization.id`, `user.account_uuid` and `user.account_id`
likewise. A `Write`'s `tool_input` contains the file's contents; an `Edit`'s
contains `old_string`/`new_string` diffs. PRD §4.1 says nothing about any of this.

**Decision.**

* Set `OTEL_METRICS_INCLUDE_ACCOUNT_UUID=false` in the injected env.
* Drop `polis_events::PII_ATTRIBUTES` at ingest, before anything is stored.
* Refuse `polis_events::REFUSED_EVENT_NAMES` (`api_request_body`,
  `api_response_body`) **by name** at the parser boundary.
* Extract `file_path` from `tool_input`, then discard the raw JSON.
* Leave `OTEL_LOG_USER_PROMPTS` and `OTEL_LOG_ASSISTANT_RESPONSES` off.
* Bind every listener to `127.0.0.1` only.
* The SQLite corpus store (PRD §6.1) holds paths and counts, nothing else.

**Consequences.** Polis needs a stable agent identity, not an identity document;
`session.id` plus `user.id` suffice. Anything persisted verbatim is a
data-retention problem in a product PRD §2 defines as local-only.

---

## ADR-0006 — Enable the beta traces channel, and define the degraded mode

**Context.** This is the single largest architectural finding. On the OTLP
**logs** channel, a subagent's tool call is byte-for-byte indistinguishable from
the main agent's: a main-agent `Read` and a subagent `Read` of the same file
produced structurally identical `claude_code.tool_result` records — same
`session.id`, same `prompt.id`, no agent field. Mechanically, `agent_id` occurs
**0 times** and `query_source` occurs **0 times** across every `tool_result` and
`tool_decision` in the capture. `subagent_completed` gives only aggregates.

Enabling the beta traces channel fixes it two ways. `agent_id` appears on the
`claude_code.tool` **span** (present for the subagent, absent for the main agent
— absence *is* the main-agent signal), and enabling traces also retro-fits
`trace_id`/`span_id` onto the log records, so the subagent's `tool_result` is
stamped with a different span.

**Decision.** Add `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1`,
`OTEL_TRACES_EXPORTER=otlp` and `OTEL_TRACES_EXPORT_INTERVAL=1000` to PRD §4.1's
env block. Register `TraceServiceServer` alongside Logs and Metrics; enable the
`trace` feature on `opentelemetry-proto`. When no spans arrive, attribute every
tool call to the main agent, set
`polis_world::Health::subagent_attribution_degraded`, and say so in the status
bar — never guess. `ChannelConfig::subagent_traces` can switch it off.

**Consequences.** A core Polis feature — the thread model of PRD §3 — now depends
on a **beta** signal Anthropic may change. `otlp-receiver.md` recommended
dropping the `trace` feature as dead weight for the receiver; that was a
receiver-scoped call, and this is the product-level one. It supersedes it.

The only logs-only fallback is temporal nesting between the `Agent` tool's
`tool_decision` and `tool_result` sequence numbers. **Do not ship it.** It breaks
completely for background subagents (`is_async = true`) and for concurrent
subagents, whose windows overlap while the main agent keeps working inside them.

Do **not** enable `ENABLE_BETA_TRACING_DETAILED` / `BETA_TRACING_ENDPOINT`: that
pair redirects logs *and* traces to a different endpoint and needs org
allowlisting.

---

## ADR-0007 — Counters are DELTA; accumulate, and read temporality off the wire

**Context.** All eight Claude Code metrics decode as monotonic `Sum`s with
`AGGREGATION_TEMPORALITY_DELTA`. Each export is the increment since the last, not
a running total. `claude_code.active_time.total` is a counter despite the name.
PRD §4.1 says nothing about temporality.

**Decision.** Accumulate per `(session.id, metric, attribute set)`. Pin
`OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE=delta` in the injected env,
**and still branch on the `aggregation_temporality` field of each metric** —
managed settings can flip it. `polis_events::Temporality::Unspecified` is treated
as Delta and raises `ControlEvent::SchemaDrift`.

**Consequences.** This is the easiest bug on this channel to ship unnoticed:
reading a delta stream as cumulative does not fail loudly, it silently reduces
every counter to "the last export interval" and still looks plausible.

---

## ADR-0008 — The bus drops the *oldest*, which crossbeam does not do for you

**Context.** PRD §4.5 specifies a bounded crossbeam channel that drops the oldest
on full. `try_send` on a full bounded crossbeam channel drops the **newest** and
returns `Full(msg)`.

**Decision.** The sink holds a `Receiver` clone and calls `try_recv()` to evict
before retrying — sound because crossbeam channels are MPMC — with the retry
capped at two iterations. The mechanism is documented in `polis_ingest::bus` so
nobody "simplifies" it back to a plain `try_send`.

**Consequences.** The handler always returns `Ok` promptly and never awaits a
send. Returning an error `Status` would make the OTel exporter retry, which is
backpressure with extra steps, and PRD §4.5 is absolute that backpressure must
never reach an agent. Measured under a permanently full capacity-8 queue with no
consumer: 2 000 RPCs, p99 handler latency 99 µs, zero errors, exact drop
accounting (3 996 dropped + 8 queued = 4 004 records).

---

## ADR-0009 — `polis-hook` installs a panic hook and uses `args_os` / `var_os`

**Context.** PRD §4.2 requires *"Never exit non-zero. Exit 0 always, even on
internal error."* That is not achievable by intent alone. Under
`panic = "abort"` on Windows a panic terminates via `__fastfail` and the parent
observes exit **0xC0000409** (3 221 226 505) — measured for `Vec` index
out-of-bounds, `None.unwrap()` and explicit `panic!()`. Separately,
`std::env::args()` aborts on a non-Unicode argv element: measured
0xC0000409 with an unpaired UTF-16 surrogate. Node cannot even reproduce that
case, because it sanitises lone surrogates; the probe built the `OsString` with
`OsStringExt::from_wide`.

**Decision.** `std::panic::set_hook(Box::new(|_| std::process::exit(0)))` is the
**first statement** of `main`. `args_os()` and `var_os()` are used exclusively.
Both defences are in: relying on the panic hook to catch a foreseeable input is
not a design.

**Consequences.** A non-zero exit here cancels a real agent's tool call. Measured
cost of the panic hook is below the noise floor. PRD §16's hook-safety list
should replace `SIGPIPE` — not a Windows concept — with: stdin closed
immediately, stdin absent (`NUL`), stdin never closed, missing endpoint file,
corrupt endpoint file, garbage port, no `--event`, unknown event name, `--event`
with no value, non-Unicode `--event` value, and induced panic. Rows for argv
robustness and induced panic are the two that actually found defects.

---

## ADR-0010 — The hook wire header: little-endian, bit 31 truncation, this-datagram length

**Context.** PRD §4.2 step 2 specifies an 8-byte header of `u32` tag and `u32`
length, and leaves three things unspecified that are each a latent cross-crate
bug: endianness, where a truncation flag lives, and whether the length is the
original stdin size or the bytes in this datagram.

**Decision.**

```text
offset 0   u32 LE  tag   bits 0..=30 event kind (0 = Unknown), bit 31 = TRUNCATED
offset 4   u32 LE  len   payload bytes in THIS datagram
offset 8   ..len   payload (raw hook stdin, unparsed)
```

Little-endian by decision, not convention: this is a same-machine loopback
protocol and every target is little-endian, so both ends use
`to_le_bytes`/`from_le_bytes` with no byte swap. The original stdin size is
deliberately unrecoverable. In place of a magic number the daemon validates
`datagram_len >= 8`, `len + 8 == datagram_len`, and `kind <= 19`.

**Consequences.** `polis-hook` cannot depend on `polis-events` (ADR-0036), so the
tag table is duplicated. `polis_events::kind::tests::hook_binary_table_matches`
parses `polis-hook/src/main.rs` with `include_str!` — a compile-time file read,
not a dependency — and diffs the two tables, plus a second test pinning the five
wire constants. That pair is the only thing standing between a rename and silent
misrouting of every hook event on the machine.

The decoder is the trust boundary for an unauthenticated port: it validates
before it slices, widens `len` to `u64` before adding (an attacker-controlled
`0xFFFF_FFF8` would otherwise wrap to 0), and is exercised against 20 000
pseudo-random buffers plus hand-picked adversarial shapes.

---

## ADR-0011 — Every channel is independently optional; port 4317 in use is degraded, not fatal

**Context.** PRD §4.1 does not say what happens when 4317 is already bound. It is
a routine state — a stale collector, a second Polis, an orphaned probe — and it
was hit for real during recon.

**Decision.** Bind `std::net::TcpListener` **synchronously on the calling
thread**, then hand it to tokio with `from_std` inside the runtime. On failure,
log a warning, emit `ControlEvent::ChannelDegraded`, and continue without Channel
A. Never panic, never retry-loop, and **never pick a different port**: agents are
configured to talk to 4317, so another port yields an empty city that looks
healthy. Match on `io::ErrorKind::AddrInUse`, never on the message text — the
Windows error string is localised (it is German on this machine).

The hook listener is the one exception: `AddrInUse` on 45177 means another Polis
is already running, and two daemons would each see half the events, so that is a
startup error with a clear message.

**Consequences.** A Polis that dies over a stale collector is strictly worse than
a Polis with no OTel. Binding eagerly on the caller's thread is the whole trick
that turns `AddrInUse` into a matchable `io::Error` rather than a panic inside a
background task nobody joins.

---

## ADR-0012 — `polis-render` and `polis-app` take wgpu and winit through eframe

**Context.** PRD §13 says "Rust + `wgpu`, native, with `winit`", implying Polis
picks its own versions. eframe 0.36.1 controls both transitively — wgpu 30.0.1,
winit **0.30.13**, not the 0.31.0-beta.2 on crates.io — and re-exports
`egui`, `egui_wgpu` and `wgpu` from its root
(`eframe/src/lib.rs:162`).

**Decision.** Neither `wgpu` nor `winit` appears in `[workspace.dependencies]` or
in any member manifest. `polis-render` writes `use eframe::{egui, egui_wgpu,
wgpu};`, including for `wgpu::util::DeviceExt`. Proven by converting the probe to
that form and recompiling; it cut the manifest to six dependencies.

**Consequences.** A direct `wgpu = "30.0.1"` silently resolves to a *different*
wgpu the day eframe bumps, and the resulting `Device`/`Queue` type mismatch is a
notoriously opaque error. Note that `glow 0.17.0` in the tree is **not** eframe's
glow renderer — it arrives via `wgpu-hal`'s GLES backend — so its presence does
not mean you are off the wgpu path.

PRD §14 also lists `polis-render` as if it were independent of the UI shell. It
cannot be: the offscreen density pass can only be encoded from inside
`egui_wgpu::CallbackTrait::prepare()` (you cannot nest a render pass inside
egui's), and `paint()` receives `RenderPass<'static>`, so GPU resources must live
in egui-wgpu's `CallbackResources` type-map. The crate's public surface is shaped
accordingly.

---

## ADR-0013 — A session is a forest, and subagent transcripts live deeper than §4.4 says

**Context.** PRD §4.4 models a session as
`~/.claude/projects/<munged-cwd>/<session-id>.jsonl` with records chained by
`parentUuid`, "so a session is a tree, not a stream". Measured over 866 files:

* **643 of 811 files (79%) are subagent transcripts**, at
  `<munged-cwd>/<session-id>/subagents/[workflows/wf_<runId>/]agent-<agentId>.jsonl`
  — variable depth, depending on how the agent was spawned.
* `parentUuid` is null on the first record of **all 643** subagent files, and no
  `uuid` edge ever crosses between files.
* Only 4 of the 19 record types are threaded at all; **~25% of lines have no
  `uuid`**.
* Two of 808 files have **zero** root records: resumed sessions whose head lives
  in the ancestor file named by the snake-case `session_id`.
* After a `system`/`compact_boundary` record, the pre-compaction thread is
  reachable only via `logicalParentUuid`.

**Decision.** Model a session as a **forest**: one tree per transcript file,
interleaved with flat, unthreaded session-sidecar records. Cross-file parenthood
comes from `agent-<id>.meta.json`, not from `parentUuid`. Watch the
`<session-id>/` sidecar *directory*, not just the file — subagent files appear at
runtime — and **glob for `agent-*.jsonl`**; never assume a depth. Prefer
`SubagentStop`'s `agent_transcript_path`, which is authoritative and arrives with
`agent_id` already attached.

Parent resolution is ranked, because picking the easiest route is the trap:

1. `meta.json`'s `toolUseId` — **140/140**, but only if `tool_use` ids are indexed
   **session-wide**. Per-file indexing gives 117/140 and looks like the format is
   unreliable; the 23 misses are nested subagents whose spawning call lives in a
   parent *subagent* file.
2. `toolUseResult.agentId` in the parent — 117/140.
3. `promptId` — 616/641. A grouping key, **not** a parent pointer.

Workflow subagents (503 of 643) have neither `toolUseId` nor `parentAgentId`;
they link structurally via `wf_<runId>` matched against the parent `Workflow`
call's `toolUseResult.runId` (40/40 on disk, 0 orphans either way).

**Consequences.** A parser that assumes every line has a `uuid` breaks on a
quarter of the corpus. A parser that follows `parentUuid` alone renders a
compacted session as two unrelated trees. A parser that assumes a root exists
fails on two real files.

---

## ADR-0014 — Order by receipt; transcript timestamps are display-only

**Context.** PRD §4.3 attributes filesystem writes by "correlating write events
against the tool-call stream within a ±2s window". Transcript timestamps are
**not monotonic**: 162 of 808 files (20%) contain at least one backwards step,
the worst file has 189, and one observed jump was **60 seconds backwards** on an
`assistant` record. Separately, OTLP `event.timestamp` is millisecond-resolution
with real ties, and OTLP spans arrive **before their parents** — batches are not
topologically ordered.

**Decision.**

* Bus order is `EventMeta::observed`, stamped when Polis receives the datagram or
  batch.
* Within a transcript file, order is `(file, byte_offset)`. Timestamps are for
  display only.
* Within an OTel session, order is `event.sequence`, not `event.timestamp`.
* The ±2 s FS-correlation window is anchored on the **hook or OTel clock**, which
  is emitted at the moment of the call.
* Buffer spans by `span_id` and resolve parentage lazily; a receiver that assumes
  a parent already exists will drop spans.

**Consequences.** A window keyed on transcript timestamps mis-attributes on a
fifth of real sessions.

---

## ADR-0015 — Coerce attributes by key, accepting any scalar variant

**Context.** PRD §4.1's defensiveness rule — unknown *event types* are logged and
dropped — is scoped one level too high. The real fragility is at the value layer.
Measured on live traffic: `duration_ms` is `string_value` on
`mcp_server_connection` and an int on `tool_result`; `prompt_length` is a string;
`safe_mode` is the string `"false"` while `has_hooks`, `has_mcp` and
`host_owned_mcp` are real `bool_value` **in the same event**; `success` and
`is_async` are the strings `"true"`/`"false"`; `duration_ms` arrived as an OTLP
`DoubleValue` in one capture and as integers elsewhere.

**Decision.** Render every `AnyValue` variant to a string first — including the
profiling-only `string_value_strindex` arm and the absent case — then coerce by
key with `as_f64` / `as_bool` helpers that accept any scalar spelling. Extend
PRD §4.1's rule to attributes: **every field is `Option`, absence is never fatal,
and absence is sometimes load-bearing** (no `agent_id` means *main agent*, not
*parse failed*).

**Consequences.** A decoder that matches one variant per key loses fields with no
error and no log line. `polis_ingest::otel::variant_names` exists as a diagnostic
to re-run after any Claude Code upgrade to catch wire-type drift.

---

## ADR-0016 — Hooks are registered in exec form, never a shell command

**Context.** PRD §4.2 does not say how the hook is registered. Measured p50 per
event, 250 samples after 15 warmups:

| Registration form | p50 |
|---|---:|
| exec form (`"args": [...]`) | **5.068 ms** |
| Git Bash `-c` | 30.820 ms (6.1×) |
| PowerShell `-Command` | 123.941 ms (24.5×) |

PowerShell is the default shell on a Windows box without Git Bash.

**Decision.** `polis install-hooks` writes exec form with
`"args": ["--event", "<Name>"]` and an explicit `"timeout": 5`. This is a
normative constraint in PRD §4.2, not a tip in the install section, and it earns
a row in PRD §13.1.

**Consequences.** The registration form costs more than everything the binary
does: shaving the in-process 0.9 ms is worth nothing next to a registration
mistake that costs 25× more. The 600-second default timeout would let a wedged
hook occupy a slot for ten minutes; 5 s is 400× the measured p95.

---

## ADR-0017 — `@`-referenced files are invisible to territory inference

**Context.** Files referenced with `@` in a prompt are added to context **with no
tool call at all** — no `Read`, and no `PreToolUse` hook fires for them, including
hooks matching `Read`. Operator-pinned files therefore never appear as
observations. PRD §6.1 assumes tool calls are the whole evidence stream.

**Decision.** Record this as a known blind spot in PRD §6.1. The three places
these files do surface are `UserPromptSubmit`'s `prompt` field, the OTel
`at_mention` event (which is why `OtelEvent::AtMention` exists), and `attachment`
transcript records (`type: "file"` and `type: "nested_memory"`).

**Consequences.** A territory that ignores `@`-mentions systematically
under-weights exactly the files the operator considered most relevant.

---

## ADR-0018 — A detached `HEAD` is not a branch for contention tiering

**Context.** PRD §11.3 tiers contention on "same branch" versus "different
worktrees/branches". 4 463 records in the local corpus carry
`gitBranch = "HEAD"` — a detached head.

**Decision.** `Claim::branch` is `Option<String>`, with `None` for detached. Two
claims both reporting `HEAD` are **not** treated as the same branch.

**Consequences.** Without this, every pair of detached-head agents would be
tiered Critical or High rather than Medium.

---

## ADR-0019 — `async_launched` is not completion

**Context.** As of v2.1.198 subagents run in the background by default. 58 of 169
`Agent` calls in the local corpus returned `status: "async_launched"` immediately
while the subagent file kept growing; completion arrives much later as a separate
`user` record with `origin.kind == "task-notification"`.

**Decision.** `Worker::running` stays true until an explicit completion signal.
`async_launched` is a launch acknowledgement, never a terminal state.

**Consequences.** A tailer that treats `async_launched` as terminal shows agents
finishing before they start.

---

## ADR-0020 — The density field is unbounded; iso thresholds use a fixed reference

**Context.** PRD §10.4 specifies additive Gaussian splats into an R16F texture,
then thresholding. Additive blending makes the field **unbounded**: measured
maximum **2.5371** with 1 750 texels above 1.0, because one full-weight Gaussian
peaks at `exp(0) - exp(-4.5) = 0.98889` and N overlapping kernels sum to ~N. The
GPU field was cross-checked against an independent CPU model of the same field
and matched to four decimals, proving nothing clamps.

**Decision.** Thresholds live in an `IsoParams` uniform and are applied against a
**fixed per-kernel reference** (`inv_scale = 1.0`, meaning "one full-weight kernel
== 1.0"), letting hot spots clip into the core band. Explicitly **not**
normalised against the observed field maximum. Uniforms rather than WGSL
constants, so retuning does not force a pipeline rebuild.

**Consequences.** Rendered both ways: normalising by the observed maximum
collapsed every ordinary territory into a single fringe band (body 1 725 px, core
422 px) — exactly the flat mush §10.4 forbids. The fixed reference gave a healthy
fringe 17 892 / body 11 063 / core 2 296 spread. Absolute thresholds now mean
"number of overlapping kernels", which is worth stating in §10.4.

The shipped values `0.08 / 0.30 / 0.75` and the −4.5 falloff constant are probe
values tuned to a synthetic ten-kernel scene, not to real KDE output.

---

## ADR-0021 — Colour space is unresolved; settle it before any §10.3 tuning

**Context.** eframe's wgpu surface is **non-sRGB and backend-dependent**:
`Rgba8Unorm` under Vulkan, `Bgra8Unorm` under DX12. An offscreen
`Rgba8UnormSrgb` target encodes differently, so identical shader output looks
visibly darker in the window than headless. PRD §10.3 asks for layers 1–2 to sit
"inside roughly the bottom fifth of the contrast range".

**Decision.** Pin a colour-space convention for custom callbacks **before** any
§10.3 contrast tuning, and always read `render_state.target_format` at runtime
rather than assuming it.

**Consequences.** Left open deliberately. Doing the palette work first means
doing it twice.

---

## ADR-0022 — Loopback UDP on every platform; no AF_UNIX anywhere

**Context.** PRD §4.2 step 3 mandates a non-blocking `sendto()` on a **Unix
datagram socket** at `$XDG_RUNTIME_DIR/polis.sock`. Measured by direct `ws2_32`
`socket()` calls: `AF_UNIX` + `SOCK_DGRAM` **fails at socket creation** on Windows
with `WSAEAFNOSUPPORT` (10047) — Windows' AF_UNIX support is stream-only.
`AF_UNIX` + `SOCK_STREAM` succeeds. `$XDG_RUNTIME_DIR` is unset on this machine.
Both halves of the step fail.

**Decision.** Loopback UDP on **every** platform. Not AF_UNIX behind
`#[cfg(unix)]`.

**Consequences.** A dual transport would mean two send paths, two error
taxonomies, two receiver implementations, two stale-endpoint stories, and
decisively two safety matrices — and PRD §16's safety assertions are the
load-bearing test in this component. Every property AF_UNIX would have bought is
already held: filesystem ACLs are matched by the per-user endpoint directory;
"cannot leave the machine" is *measured* (a socket bound to `127.0.0.1` returns
`NetworkUnreachable`, raw 10051, for any off-box destination) and reinforced by
`parse_endpoint` accepting only literal IPv4 loopback; port collision is solved by
a fixed port outside the ephemeral range; and UDP has no filesystem node to go
stale. A dual transport would additionally import macOS's
`net.local.dgram.maxdgram` trap, where a 60 KiB frame that works on Linux fails.

Also correct §4.2's "Never a FIFO" rationale: on Windows the equivalent hazard is
a **named pipe** (`\\.\pipe\...`), where `CreateFile` blocks pending a server
instance. The warning is right; the mechanism name needs updating.

`WSAECONNRESET` did not reproduce on this machine in any configuration, including
the connected-send case that is supposed to trigger it — most likely the local
firewall suppressing the inbound ICMP, which is a property of this box and not of
Windows. So the neutralisation is argued **structurally**: never `connect()`, one
socket per process destroyed before feedback could land, and the send result
discarded. `SIO_UDP_CONNRESET = 0` is applied on the daemon's receive socket as
defence in depth.

---

## ADR-0023 — 60 KiB send cap with a truncation flag, not 256 KiB

**Context.** PRD §4.2 step 1 caps stdin at 256 KiB. A UDP datagram carries at most
**65 507** payload bytes; 65 508 fails with `WSAEMSGSIZE` (10040). Because the
hook correctly discards the send error and exits 0, an over-cap event would vanish
**silently** — and large `PermissionRequest` and `PreToolUse` payloads are exactly
the events most worth seeing. `SO_SNDBUF` does not bound datagram size: a
61 448-byte datagram was delivered with `SO_SNDBUF` set to 0, 1024, 8192, 32768
and 65536.

**Decision.** `MAX_PAYLOAD = 61 440` (60 KiB), framed to 61 448 bytes. Read
`MAX_PAYLOAD + 1` to detect overflow without a second syscall, truncate, set tag
bit 31, **send, and only then drain the remainder of stdin**.

**Consequences.** 4 059 bytes of slack absorbs future header growth without a
wire break and stays clear of the exact boundary. Send-before-drain means a
32 MiB payload delays delivery by nothing, and the watchdog bounds the drain. The
drain exists so the agent's write always completes: verified from a Node parent
watching for a stdin `error` event across 0, 1024, 61440, 61441, 102400, 262144,
1 MiB, 8 MiB and 32 MiB — exit 0, no signal, no parent error, every time.

Verified on the wire: 61 440 B stdin → tag `0x00000009`, trunc 0; 61 441 B → tag
`0x80000005`, trunc 1; both datagrams 61 448 bytes. Re-verified against the built
binary in this workspace: 60 KiB + 1 → 61 448-byte datagram with bit 31 set.

The daemon treats a truncated event as **notification-only** and backfills detail
from the JSONL transcript.

---

## ADR-0024 — Split the 3 ms budget into three rows

**Context.** PRD §4.2 and §13.1 both set "`polis-hook` p99 wall time: 3 ms" as a
single end-to-end number. It is unreachable, and not because of anything in the
binary. Measured, n = 400 each, interleaved round-robin, warmups discarded:

| Measurement | p50 | p99 |
|---|---:|---:|
| in-process (stdin → framed → sent → exit), worst config | 0.911 ms | **1.234 ms** |
| `noophook` (`fn main(){exit(0)}`) from a **Rust** parent | 3.300 ms | 3.997 ms |
| `polis-hook` from a **Rust** parent | 4.735 ms | 5.483 ms |
| `noophook` from a **Node** parent | 10.635 ms | **13.810 ms** |
| `polis-hook` from a **Node** parent | 14.547 ms | 22.147 ms |

Claude Code is Node. ~80% of the end-to-end cost is Node's own `child_process`
machinery; Polis's marginal contribution is 2.5–3.9 ms.

**Decision.** Replace the single row with three:

1. **In-process p99 ≤ 3 ms.** Met with 2.4× margin. This is the CI gate — the
   only half Polis can regress.
2. **End-to-end spawn p99 ≤ 25 ms on Windows**, with ≥ 13.8 ms documented as an
   OS + runtime floor Polis does not control.
3. **Registration form must be exec form** (ADR-0016).

Linux and macOS budgets are to be established on those CI legs, **not**
extrapolated: `fork+exec` and `CreateProcess` are not comparable, and nothing
non-Windows was executed.

**Consequences.** This strengthens §4.2's own argument for keeping hooks rare: the
floor is ~13 ms per event regardless of binary quality, so the OTel channel
carries even more weight than assumed. `UdpSocket::bind` dominates the in-process
cost (0.739 ms of the 0.894 ms p50) — that is Winsock initialisation on first
socket use, not the bind.

Anyone re-running these numbers must state which parent they used, and must
interleave variants round-robin: a block-per-variant layout on this machine
attributed bursty background load to whichever variant was unlucky and produced a
114 ms p99 for a binary whose entire body is `exit(0)`. Discard 15–25 warmup
spawns: a freshly built executable's first runs are 6–24× slower while antivirus
scans the image.

---

## ADR-0025 — A 500 ms watchdog thread

**Context.** PRD §4.2 requires "Never blocks" but addresses only the socket send.
Measured, loopback UDP send never parks and never returns `EAGAIN` even at 20 000
sends from one socket. The real risk is `read(stdin)` never reaching EOF, which
blocks a real tool call for Claude Code's full 5 s hook timeout.

**Decision.** A thread spawned at startup sleeps 500 ms and calls `exit(0)`.

**Consequences.** Caps the worst case at a tenth of the hook timeout. Verified
firing at 505 ms (Rust parent) / 518 ms (Node parent) with stdin held open.
Marginal cost measured at −0.022 ms p50, i.e. below the noise floor.

Known risk: if a future Claude Code writes hook stdin slowly enough, this
truncates a legitimate payload. 32 MiB completes well inside 500 ms today, so the
headroom is large — but if the daemon starts seeing unexplained `TRUNCATED`
events with small payloads, this constant is the first suspect.

---

## ADR-0026 — Fixed port 45177; the bind is the singleton lock; endpoint-file discovery

**Context.** PRD §4.2 gives no port and no discovery mechanism.

**Decision.** Discovery order, cheapest first: `POLIS_HOOK_ENDPOINT` →
`%LOCALAPPDATA%\polis\endpoint` (Windows) or `$XDG_RUNTIME_DIR/polis/endpoint`
with `$XDG_STATE_HOME` and `~/.local/state` fallbacks (Unix) → compiled-in
`127.0.0.1:45177`. The file read is bounded to 128 bytes, first line only,
`from_utf8_lossy`, and parsed as a **literal IPv4 loopback address only**.

The daemon always binds the fixed port. `bind(127.0.0.1:45177)` **is** the
singleton lock; on `AddrInUse` the second daemon exits with a clear message.
Stale files need no cleanup: write atomically (temp + rename), delete
best-effort on clean shutdown.

**Consequences.** Measured, the endpoint file costs 17 µs p50 against a ~5 ms
spawn floor, and the three resolution paths are statistically indistinguishable
end-to-end — so the file is free and stays the primary zero-config mechanism. The
env var cannot be primary: it only reaches agents Polis launched, and operators
start `claude` from their own terminal.

`ToSocketAddrs` is refused: `localhost:45177` costs p99 0.522 ms and a failing
name up to 4.193 ms — more than the entire in-process budget — for a value that
is always `127.0.0.0/8`. Literal-only parsing is also what confines a tampered
endpoint file to loopback.

45177 is below the OS ephemeral range (measured 49 152+ via `netsh`, confirmed by
64 sample binds landing in 57132..57195), so it can never be handed to an
anonymous client socket. Had the daemon used an ephemeral port recorded in the
file, a crash could leave the file pointing at a port the OS later reassigns to
an unrelated local process — and hook payloads contain source code. A stale file
is harmless: sending to a closed port is measured as `Ok(n)`, exit 0, and
marginally *cheaper* than the live case.

Silently taking a different port is the failure that produces a half-populated
map and an operator who cannot tell which window is lying. A `0.0.0.0:45177`
squatter neither blocks the loopback bind nor steals the traffic.

---

## ADR-0027 — Daemon receive-socket obligations

**Context.** PRD §4.2 and §4.5 describe the hook side only. At the default
`SO_RCVBUF` of 65 536, a burst of 5 000 × 60 KiB datagrams loses **0.9%**; at
8 MiB and 32 MiB it loses 0%. PRD §13.1 asks for 500 events/sec with zero drops.

**Decision.** `polis-ingest`'s hook listener must: set `SO_RCVBUF = 8 MiB`; set
`SIO_UDP_CONNRESET = 0`; bind `127.0.0.1:45177` exclusively and treat `AddrInUse`
as "another Polis is running"; use a `recv_from` buffer ≥ 65 536; validate
`len + 8 == datagram_len` and `kind <= 19` before trusting a frame; treat the
argv tag as a hint and `hook_event_name` as authoritative; and treat a set
`TRUNCATED` bit as notification-only.

**Consequences.** The endpoint is unauthenticated: any local process that knows
the port can inject forged events, and the compiled-in default is public. The
blast radius is a visualisation drawing something untrue — it cannot execute
anything — and the daemon already parses defensively. Sender authentication would
require a shared secret in a longer header, i.e. a wire-format change. The
outbound direction, which matters more because payloads contain source code, **is**
closed and measured.

---

## ADR-0028 — `LogicalPath` folds ASCII case on every platform

**Context.** PRD §7.6 makes the logical path the layout key and says deciding it
late is painful. Four channels spell paths differently: transcript tool inputs are
backslash 11 606 / forward 1 268 — **both**; `cwd` is backslash 125 050/125 050;
OTel `tool_input` carries JSON-escaped separators that must be decoded, not
sliced; the filesystem watcher returns whatever the OS gives, including `\\?\`
verbatim prefixes.

**Decision.** Forward slashes always. `..` resolved **lexically**, never against
the filesystem (the file may not exist — `PreToolUse` fires before the write — so
`fs::canonicalize` is unavailable), and a path escaping its root is rejected
rather than clamped. `Eq`, `Ord` and `Hash` fold **ASCII** case identically on
every platform; display casing is preserved so PRD §12's "click a building → open
in `$EDITOR`" still opens the real file. Non-UTF-8 paths are **rejected**, not
lossily converted. The worktree prefix is stripped by `PathMapper`, which returns
the `WorktreeId` alongside — never folded into the path.

**Consequences.** Case folding is not platform-dependent, deliberately: PRD §16
runs the golden-layout test on two operating systems and PRD §7.4 requires
byte-identical output, so a key that folds on Windows and not on Linux would fail
that test for a reason unrelated to layout. ASCII-only is also deliberate:
Unicode case folding is Unicode-version dependent and would make the city move
when the toolchain moves.

`to_string_lossy` is refused because it maps two different files onto one key via
`U+FFFD`, which is worse than skipping them. There is deliberately no
`Borrow<str>` impl: `str`'s `Hash` does not fold case, so
`HashMap::<LogicalPath, _>::get(&str)` would silently miss.

**Known limitation:** no Unicode normalisation. A file created on macOS as `café`
in NFD and referenced on Windows in NFC yields two logical paths. Fixing it needs
a dependency whose tables change between releases, which would break PRD §7.4.

---

## ADR-0029 — Determinism is written out, never delegated to a crate default

**Context.** PRD §7.4 promises "the same repo produces the same city on every
launch **and on every machine**". `std::hash::DefaultHasher`'s algorithm is
explicitly not guaranteed stable across Rust releases, and a `rand` generator can
change its stream in a patch release.

**Decision.** `LogicalPath::layout_seed` is FNV-1a over the case-folded bytes,
written out and **pinned by a test with literal expected values**.
`polis_layout::determinism::SeededRng` is likewise a written-out generator, seeded
per draw site from `(path, purpose)` so adding a draw somewhere in the pipeline
cannot shift every later draw. Coordinates are quantised before serialisation, so
the two-OS golden-file comparison cannot fail on a last-bit floating-point
difference. `BTreeMap` everywhere iteration order can reach the layout.

**Consequences.** If the pinned seed test ever fails, every golden layout file in
the repo is invalidated **on purpose** — that is the signal, not a nuisance.

---

## ADR-0030 — The thread key is `(session_id, agent_id)`

**Context.** PRD §5 keys `World::threads` on `ThreadId` without saying what that
is. Measured: a subagent transcript's `sessionId` is the **parent's**, identical
across every subagent of that session, on **61 239 of 61 239** records. The hooks
channel agrees. Separately, `attributionAgent` looks like a thread identifier and
is not — it holds the agent *type* (`workflow-subagent` 23 265, `general-purpose`
6 070, `Explore` 5 915, `Plan` 1 702) and never equals an `agentId` in 36 952
comparisons.

**Decision.** Thread key is `(session_id, agent_id)`, with `agent_id` absent
meaning the main agent. `is_worker` is keyed on **`agent_id` presence only** — a
main agent launched with `claude --agent foo` also carries an `agent_type`.
`AgentType` and `WorkerId` are separate types in `polis-events` so the two cannot
be confused at a call site.

**Consequences.** Keying threads on `sessionId` alone merges every subagent of a
session into one thread.

---

## ADR-0031 — There is no `Task` tool and no `MultiEdit` tool

**Context.** The PRD implies the subagent-spawning tool is called `Task`. Across
37 917 `tool_use` blocks there are **zero** calls to a tool named `Task`; the
subagent-spawning tool is **`Agent`** (169 calls; inputs `description`,
`subagent_type`, `prompt`, `run_in_background`, `model`).
`TaskCreate`/`TaskUpdate`/`TaskGet`/`TaskOutput`/`TaskStop` are an unrelated to-do
list. Independently, the hooks reference confirms the file-mutating built-ins are
exactly `Edit`, `Write` and `NotebookEdit`: there is **no `MultiEdit`**.

**Decision.** Rename every PRD reference to `Task` → `Agent`. Neither name is
special-cased anywhere; `tool::tests::tools_that_do_not_exist_are_not_special_cased`
asserts both fall through to `ToolKind::Other`.

**Consequences.** A matcher on a tool that does not exist silently matches
nothing. Audit every matcher and attribution rule for tool names.

---

## ADR-0032 — `Bash` and `PowerShell` are both shells

**Context.** PRD §6.1's evidence table has a "Bash cwd" row. On Windows with the
PowerShell tool enabled, shell commands route through `PowerShell`, and **without
Git Bash, Claude Code does not register the `Bash` tool at all**. Local corpus:
`Bash` 16 416 calls, `PowerShell` 2 137.

**Decision.** §6.1's row reads "`Bash` **or** `PowerShell`", both weighted 0.5.
Every matcher accepts `Bash|PowerShell`. `ToolKind::is_shell()` exists precisely
so no call site has to remember.

**Consequences.** Testing "is this a command" against `Bash` alone silently
ignores 11.5% of shell calls here, and 100% of them on a Windows box without Git
Bash.

---

## ADR-0033 — The project key must be discovered, not only computed

**Context.** PRD §4.4 describes the munged-cwd rule correctly but omits three
details that produce the wrong directory if guessed: the hash is over the
**original** `cwd`, not the munged string; it is a JavaScript 32-bit
`h = (h << 5) - h + c` over **UTF-16 code units** rendered base-36; and
`Math.abs` on `i32::MIN` yields 2 147 483 648 in JavaScript, so Rust must widen
to `i64` before taking the absolute value or it panics. Verified byte-for-byte
against the live CLI with a 218-character `cwd`. Additionally, the CLI computes
`projectKey` as `override() ?? munge(cwd)` — the override is guarded by a
filename-safety regex and a Windows reserved-name check.

**Decision.** Implement the exact algorithm **and** watch `~/.claude/projects/`
for new directories, reading `cwd` out of the records to learn the real mapping.

**Consequences.** A session's directory is not always derivable from `cwd`.
Compute-only discovery misses any session started with an overridden key.

---

## ADR-0034 — Windows `MAX_PATH`: stay in `std::path`

**Context.** A `cwd` deep enough to trip the 200-character cap produces a
207-character project directory and a **282-character transcript path**, on a
machine with `LongPathsEnabled = 0`. Rust's `std::fs` read/`File::open`/`metadata`
all work (std uses `\\?\` verbatim paths internally). Python's `io.open()`
**fails** with `FileNotFoundError`.

**Decision.** All path handling stays in `std::path::Path`. Any external tool
invoked on a transcript path needs a `\\?\` prefix. `PathMapper` handles `\\?\`
and `\\?\UNC\` prefixes. Startup self-checks by actually reading a discovered
transcript before advertising the session as watchable.

**Consequences.** `notify` — PRD §4.3's watcher — is **untested** against such a
directory. `polis_ingest::fs::self_check_long_paths` exists to make that a startup
answer rather than a runtime surprise. Verify before shipping Channel C.

---

## ADR-0035 — Assert field population counts, not parse success

**Context.** PRD §4.4 mandates `{ known fields } + serde_json::Value` catch-all.
That is correct and necessary, and it is also a trap: with `Option` fields,
`#[serde(default)]` and a `#[serde(flatten)]` catch-all, **forgetting
`#[serde(rename_all = "camelCase")]` makes `toolUseResult`, `promptId`,
`requestId`, `agentId` and `isMeta` all deserialize to `None` while serde reports
no error at all**. The recon model shipped this bug and a 153 613-record corpus
test passed clean. Only asserting field population caught it.

Related sharp edge: camelCase of `source_tool_assistant_uuid` is
`sourceToolAssistantUuid`, which does not match the wire name
`sourceToolAssistantUUID`. It needs an explicit `rename`.

**Decision.** Add to PRD §16: the transcript parser's test suite asserts field
**population counts** against the real corpus, not merely parse success.

**Consequences.** The catch-all that makes the parser robust is the same thing
that makes a rename bug invisible.

---

## ADR-0036 — `polis-hook` has no dependencies at all

**Context.** PRD §14 says "no deps beyond libc + std". Measured: it is achievable
with **std alone**. `cargo tree -p polis-hook` prints exactly one line, itself.
The release binary is 148 480 bytes, of which ~100 KB is the Rust std floor — a
`fn main(){exit(0)}` binary on the identical profile is 99 840 bytes.

**Decision.** Tighten PRD §14 to "no dependencies at all; std only" and record the
size floor. `polis-hook/Cargo.toml` has an empty, commented `[dependencies]`
section, and it is built with a dedicated `[profile.hook]`
(`opt-level = "z"`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`,
`strip = "symbols"`). CI asserts the tree.

**Consequences.** Because it cannot depend on `polis-events`, the event-tag table
is duplicated and pinned by a cross-checking test (ADR-0010). A future PR adding
a "small" dependency now has a number to argue against: this binary is spawned
once per hook event on a real agent's critical path, so every initialiser it
links is on that path.

Three behaviour-preserving edits were made to the verified reference source to
keep the workspace at zero clippy warnings: `map(f).unwrap_or(0)` → `map_or`, two
`as` length casts → `u32::try_from(..).unwrap_or(MAX_PAYLOAD_U32)`, and a runtime
test converted to a `const` assertion. The built binary was re-verified
end-to-end afterwards — correct tag, correct length, truncation bit at exactly
61 440 bytes, exit 0 with no listener bound.

---

## ADR-0037 — Register three OTLP services, with an exact feature set

**Context.** PRD §4.1 says "`tonic` + `opentelemetry-proto`" without features, and
predates the tonic 0.14 split that moved prost codegen into `tonic-prost`. It is
easy to miss that `opentelemetry-proto` needs the `trace` feature, and easy to
wrongly add `tonic-prost` as a direct dependency.

**Decision.** The exact block is pinned in `[workspace.dependencies]`.
`tonic-prost`, `prost` and `prost-types` are **not** direct dependencies — the
`ProstCodec` call lives inside `opentelemetry-proto`, which carries its own
`tonic-prost` dep; proven by building the reference receiver with all three
removed. Register `LogsServiceServer`, `MetricsServiceServer` **and**
`TraceServiceServer`.

**Consequences.** Omitting the third makes Claude Code's trace exports fail
`UNIMPLEMENTED` and costs Polis its thread model (ADR-0006). `gen-tonic`
transitively pulls `opentelemetry` and `opentelemetry_sdk` and forces
`tonic/channel`; that is unavoidable in 0.32. Also filter on
`service.name == "claude-code"` — a cheap guard against foreign OTLP traffic on
an unauthenticated loopback port.

---

## ADR-0038 — `prompt.id` is a turn key, optional, and absent on startup

**Context.** PRD §4.1 lists `prompt.id` as a **VERIFY** item. Confirmed exactly:
the attribute is literally `prompt.id`, dotted, a UUID v4, present on
`user_prompt`, `tool_decision`, `tool_result` and `subagent_completed`. But it is
**absent** on startup events (`plugin_loaded`, `permission_mode_changed`, the
pre-prompt `api_request`, the session-title request) and **never** on metrics, by
design (cardinality).

**Decision.** Mark the VERIFY resolved. `PromptId` is `Option` everywhere and no
parser requires it. `session.id` is the only key present on every record
including metrics, and is therefore the session key — it is **not** a resource
attribute, despite that being the normal OTLP idiom. The resource block carries
exactly five keys: `service.name`, `service.version`, `os.type`, `os.version`,
`host.arch`.

**Consequences.** Grouping by resource cannot separate agents. Using `prompt.id`
as a join key for session-level state fails on every startup record. On the hooks
side, `prompt_id` additionally requires Claude Code v2.1.196+ and does not exist
for `SessionStart`.

---

## ADR-0039 — `event.sequence` is a free loss detector

**Context.** Every OTel event carries `event.sequence`, an integer monotonic per
session from 0. PRD §4.1 does not mention it. Also: `event.timestamp` is
millisecond-resolution with real ties (three `plugin_loaded` events sharing one
millisecond; eight metric datapoints sharing one `time_unix_nano`).

**Decision.** Track the high-water mark per session; a hole emits
`ControlEvent::SequenceGap` into the same status-bar counter as dropped events.
Order within a session by `event.sequence`, never by `event.timestamp`.

**Consequences.** The cheapest possible detector for the drops PRD §4.5 explicitly
permits.

---

## ADR-0040 — Live token display comes from `api_request`, not the metric stream

**Context.** PRD §4.1 sets `OTEL_METRIC_EXPORT_INTERVAL=10000`, so counters lag by
up to 10 seconds. The defaults are 60 000 ms for metrics and 5 000 ms for logs, so
the PRD's values are a deliberate 6× and 5× speed-up — worth stating so the
override reads as tuning rather than boilerplate. Claude Code has **no** default
OTLP protocol: without `OTEL_EXPORTER_OTLP_PROTOCOL` the otlp exporter does
nothing at all.

**Decision.** Drive any live token or cost display from the `api_request`
**event** stream (1 s cadence), which carries the same per-request token counts.
Reserve the metric stream for accumulated totals.

**Consequences.** Two keys have inconsistent meanings across signals and must not
be joined naively: `query_source` has entirely different vocabularies on metrics
(`main`/`auxiliary`/`subagent`) versus events
(`sdk`/`generate_session_title`/`agent:builtin:<type>`), and `model` carries a
`[1m]` context-window suffix on metrics but not on events.

---

## ADR-0041 — Two hook-configuration facts worth writing down

**Context.** Two minor findings that would otherwise cost an implementer an hour
each.

**Decision.**

* `suppressOutput` **does nothing**. Claude Code accepts the field and does not
  act on it. Do not reach for it.
* In an **interactive** session Claude Code runs no settings-file hook —
  *including from `~/.claude/settings.json`* — until the user accepts the
  workspace trust dialog for the folder. `polis install-hooks` must say so, in
  `HookInstall::notes`.

**Consequences.** Without the trust note, the first launch after install looks
like a silent failure.

---

## ADR-0042 — Building height comes from the transcript, not from re-diffing

**Context.** PRD §7.3 says "Height is proportional to uncommitted diff lines",
implying the renderer derives it. The transcript already carries it three ways:
`structuredPatch` gives exact ± counts per edit (38 154 added / 12 118 removed
corpus-wide, across 2 436 patches and 2 792 hunks); `toolUseResult.toolStats`
gives a per-subagent roll-up; `cost-state` gives a per-session total.
`toolUseResult.file.totalLines` also gives file length for the footprint without
a `stat()`. Bash `gitOperation` is even pre-classified into
`{commit:{sha,kind,branch}}` / `{push:…}` / `{branch:…}` — do not re-parse git
command lines.

**Decision.** Name these fields in PRD §7.3. `polis_repo::git::diff_line_counts`
is the **fallback**, used where `structuredPatch` is missing — which is every
subagent edit (ADR-0004).

**Consequences.** Nobody should re-derive the height by diffing the working tree.

---

## ADR-0043 — Stable releases only; no alpha, beta or release-candidate dependencies

**Context.** The candidate version list included `winit 0.31.0-beta.2`,
`notify 9.0.0-rc.5` and `smallvec 2.0.0-alpha`.

**Decision.** `notify` is pinned to **8.2.0** (latest stable), `smallvec` to
**1.16.0**, and `winit` is not a direct dependency at all (ADR-0012). Every pin in
`[workspace.dependencies]` is a stable release that was actually resolved and
compiled by this workspace or by a recon probe.

**Consequences.** The one beta Polis does depend on is a *signal*, not a crate —
the OTel traces channel (ADR-0006) — and that dependency is explicit, switchable,
and has a defined degraded mode.

---

## ADR-0044 — Three hook events `hooks-schema.md` §1.2 recommends are deliberately not registered

**Context.** `hooks-schema.md` §1.2 lists seven events PRD §4.2 missed "that
Polis needs". Four are registered: `PreToolUse` (ADR-0004), `Notification`,
`CwdChanged`, and `FileChanged` is rejected outright (ADR-0003). The other three
— `PostToolUse`, `UserPromptSubmit`, `PermissionDenied` — were silently absent
from both `polis_events::EventKind` and the §6 `install-hooks` template with no
recorded reason, which is the shape of an oversight rather than a decision.
`polis_world::contention::ClaimTable::release` still referred to a `PostToolUse`
hook that nothing would ever deliver.

**Decision.** All three stay unregistered, and the reason is written down:

* **`PostToolUse`** — the only §11.3 use is early claim release. `tool_result`
  on Channel A and `Modified` on Channel C both report the same landing at zero
  spawn cost, and the 30 s TTL is the backstop. Registering it would put a
  ~11–15 ms Windows process spawn (ADR-0024) on every `Edit` and `Write` — the
  highest-frequency mutating events there are — to save at most one logs-export
  interval (5 s default). Revisit only if TTL churn measurably misleads.
* **`UserPromptSubmit`** — its stated value is the `prompt_id` join key, but
  `prompt_id` is a *common* field on every hook payload (`hooks-schema.md` §2)
  and `prompt.id` is on every non-startup OTel event (ADR-0038), so the join is
  already available. It is also the one event on the list whose exit 2 **erases
  the user's prompt**, and its timeout drops to 30 s. Worst risk, no new signal.
* **`PermissionDenied`** — auto-mode denials are genuinely invisible to
  `PermissionRequest`, but `tool_decision` on Channel A carries rejections
  including `decision = reject` (otel-schema §10 row 9) with no spawn. Note that
  rejected calls are listed as unexercised in open item 9, so if `tool_decision`
  turns out not to fire for auto-mode denials, this is the first event to add.

**The tag table is append-only.** `EventKind`'s discriminants are the wire tags
`polis-hook` writes; renumbering silently misroutes every hook event on the
machine. Any of the three above is added as tag 20, 21, 22 — never inserted —
and `polis-hook`'s `event_tag` must be updated in the same commit, which
`polis_events::kind::tests::hook_binary_table_matches` enforces.

**Consequences.** 19 registrations, matching `hooks-schema.md` §6 exactly. Claim
release is a Channel A / Channel C responsibility, and `ClaimTable::release`
now says so.

---

## ADR-0045 — All five trace spans get a variant, not just `claude_code.tool`

**Context.** ADR-0006 turns the beta traces channel on because it is the only
way to distinguish a subagent's tool call. But `OtelEvent` modelled exactly one
of the five span names in `otel-schema.md` §6.1, so the other four degraded to
`OtelEvent::Unknown` — including `claude_code.llm_request`, which is one of only
**two** places `agent_id` ever appears. The channel Polis enabled specifically
for subagent attribution was throwing away half of that attribution.

**Decision.** Add `InteractionSpan`, `LlmRequestSpan`, `ToolExecutionSpan` and
`ToolBlockedOnUserSpan` alongside `ToolSpan`. Publish the dispatch tables as
`polis_events::OTEL_EVENT_NAMES` (26 log events) and
`polis_events::OTEL_SPAN_NAMES` (6 span names), so the receiver matches a table
rather than a hand-copied `match` and a schema change is one test failure rather
than a silent drop.

**Consequences.** `claude_code.hook` is in `OTEL_SPAN_NAMES` for completeness and
must stay unmodelled: capturing it needs *detailed* beta tracing, which redirects
logs **and** traces to a separate endpoint and would hijack Polis's own export
destination. `ToolBlockedOnUserSpan` is a bonus — it is the only direct
measurement of how long a thread sat in PRD §11.2 attention state (a), though its
`decision` and `source` attributes were observed as the literal string
`"unknown"` and must not be read.

---

## ADR-0046 — Path normalisation compares bytes, never `str` indices

**Context.** Two functions in `polis_events::path` derived a `str` index from a
byte length taken from a *different* string, and `str` indexing panics off a
character boundary. Both were reachable from untrusted input and both were
verified to panic:

* `LogicalPath::starts_with` — `self.0.split_at(prefix.0.len())`.
  `lp("日本").starts_with(&lp("ab"))` split byte 2 of `日`.
* `AbsPath::parse`'s verbatim-prefix probe — `rest[..4]`.
  `\\?\C:\日本\a.rs` put byte 4 inside `日`. Long paths and non-ASCII directory
  names co-occur constantly on Windows, and `\\?\` is exactly what `std` hands
  back for a path over `MAX_PATH` (`jsonl-schema.md` §1 measured a 282-character
  transcript path on this machine).

Paths reach these functions from all four channels, including the filesystem
watcher and JSON payloads, so either panic takes the ingest thread down on a
Japanese, German or emoji-named directory.

**Decision.** Both compare `&[u8]` with `<[u8]>::eq_ignore_ascii_case`. Byte
slicing cannot land mid-character. This is consistent with ADR-0028, which
already fixed comparison at *ASCII* case folding — the folding was right, the
slicing was not.

**Consequences.** No behaviour change for ASCII paths; every one of the 35
pre-existing path tests still passes. Any future comparison in this module must
use byte slices for the same reason. `drive_prefix`'s `s[..2]` is safe and stays,
because it is guarded by two `is_ascii_*` checks on the same bytes.

---

## ADR-0047 — `TranscriptSource` carries the workflow run id

**Context.** ADR-0013 established that workflow subagents — **503 of 643**
subagent transcripts — have neither `toolUseId` nor `parentAgentId`, and link to
their parent *only* by matching the `wf_<runId>` path segment against the
spawning `Workflow` call's `toolUseResult.runId` (40/40 on disk). But
`polis_events::TranscriptSource` modelled `Subagent { agent }` with no run id and
`WorkflowJournal` as a unit variant, so the one field that attributes four out of
five subagent files was structurally unrepresentable, and two concurrent workflow
runs in one session collapsed onto a single source value.

**Decision.** `Subagent` gains `workflow_run: Option<String>` (`None` for a
directly spawned agent) and `WorkflowJournal` gains `run: String`.

**Consequences.** The tailer must keep the `wf_<runId>` segment when it globs
`agent-*.jsonl`, rather than discarding path structure once it has the agent id.

---

## Open items carried forward

Not decided here, and each needs an owner:

1. **Cloud cap** (PRD §17 Q1). What is the right cap, and should dormant threads
   dissipate entirely or leave a faint residue? Configurable until answered
   against a real fleet.
2. **Is "done, unverified" a fourth attention state?** (PRD §17 Q2.) It behaves
   more like `NeedsDecision` than like `Done`.
3. **Trail time encoding** (PRD §17 Q3): dash density or opacity ramp, or is fade
   sufficient?
4. **Merge flattening** (PRD §17 Q4): height from uncommitted diff means the city
   flattens on merge. Does that destroy the "recently active" reading, and is a
   slow-decay ghost needed?
5. **Colour space** (ADR-0021), before any §10.3 tuning.
6. **`notify` against a >`MAX_PATH` directory** (ADR-0034), before shipping
   Channel C.
7. **Performance is entirely unvalidated.** The GPU probe proves the pipeline is
   *correct*, not fast: ~10 splats and 13 triangles, no MSAA, no culling, and no
   measurement against PRD §13.1's 16.6 ms frame, <4 ms agent+attention layer, or
   <2% idle CPU. Multi-agent fan-in on the OTLP receiver was never load-tested
   either — one session at a time.
8. **Everything is pinned to Claude Code 2.1.248** and to one GPU (an AMD Radeon
   RX 9070 XT). The `Rgba16Float` density fallback compiles but has never been
   exercised, and taking it requires flipping the splat fragment entry point's
   return type from `f32` to `vec4<f32>`.
9. **Unexercised on the OTel channel:** rejected tool calls
   (`decision = reject`, `user_abort`, `user_reject`), concurrent and background
   subagents, nested subagents (`parent_agent_id` was never populated),
   `Bash`/`PowerShell` `full_command`, `NotebookEdit`, MCP tools, and `Skill`.
10. **Nothing non-Windows was executed.** Every Linux and macOS statement in
    `hook-ipc.md` is unverified, including the end-to-end spawn budget. The safety
    matrix must be re-run per platform.
