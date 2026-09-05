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

**Amended: "0 orphans either way" is a statement about reading a session from
disk, and it does not survive live tailing.** The run id is matched against a
*single record* in the main transcript, and `start_discovering` follows existing
files from their end — so a Polis started after the `Workflow` call never reads
the record the whole route depends on, and every agent of that run parks
unattributed for ever, with no second chance: nothing re-reads the parent.
Observed on a live fleet as twelve rows of *"workflow run has no parent
`Workflow` call yet"* whose parent call was in the transcript the entire time.
Route 3 is also the one route with no fallback — an `Agent` spawn is recoverable
because the subagent's own transcript carries the parent's `sessionId`, and a
journal record carries no `sessionId` at all.

So there is a fourth rung, and it needs no record: the journal is read out of
`<munged-cwd>/<session-id>/subagents/workflows/wf_<runId>/`, so the **owning
session is a parent directory of the file**.
`polis_ingest::transcript::attribute_to_file` puts it on the envelope whenever
the record itself names no session, and `polis_world::apply::journal` takes it
after the run table and the worker's own transcript have both missed. It is
attributed `WorkerAttribution::TranscriptFile` — the path rather than a record
field — so `strength` keeps it from ever overwriting a real match, and it is
gated on the thread already existing, because a directory name is not proof that
a session does.

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

The same rule governs the **identity hue**: `ThreadId::hue_preference` is FNV-1a
over the session id's bytes, written out in `polis-events/src/ids.rs` and pinned
by `the_identity_hash_is_written_out_and_pinned` with literal expected values.

**Consequences.** If the pinned seed test ever fails, every golden layout file in
the repo is invalidated **on purpose** — that is the signal, not a nuisance.

**Amended by ADR-0101 — the identity hue is no longer deterministic per id.**
Anyone reading this ADR and assuming that a `ThreadId` determines its colour the
way a `LogicalPath` determines its seed would be wrong, and it matters, because
the hue is the one place in the product where a written-out hash stopped being
the last word. The *preference* is still a pure function of the id and is still
pinned by literals; the *slot actually drawn* is assigned by `polis_world::World`
and is a function of the ordered set of distinct thread ids the world has seen.
That is a genuine weakening of what this ADR promised, and it buys the thing the
hash could not: two live threads are never the same colour. It is deterministic
for a given event sequence — which is what `step`, `seek`, `run_to_end` and every
golden test give it — rather than for a given id in isolation.

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

## ADR-0048 — The drift-carrying enums are `#[non_exhaustive]`

**Context.** Seven enums in `polis-events` model schemas Polis does not own:
`EventKind` (hook events), `ToolKind` (Claude Code's tool set), `OtelEvent` (26
beta log events plus five span names), `TranscriptRecordKind` (19 undocumented
record types), `FsEvent` (`notify`'s vocabulary), `Payload` (the four channels
plus control) and `ControlEvent` (Polis's own failure modes). Every one of them
is expected to gain members — ADR-0044 states outright that the `EventKind` tag
table is **append-only**, `docs/verified/otel-schema.md` is explicit that the
OTel schema moves, and the transcript format is undocumented internals.

Each already carries an escape variant — `Unknown`, `Other`, `Unspecified` — so
a *runtime* addition degrades gracefully. The compile-time side was not covered:
an exhaustive `match` in any of the seven downstream crates turns "Claude Code
2.1.260 added an event" into a workspace-wide compile break, which is the exact
opposite of the "degrade, never fail" posture PRD §4.1, §4.4 and §17 all
mandate. With eight crates fanning out in parallel, the number of exhaustive
matches only grows.

**Decision.** All seven carry `#[non_exhaustive]`. `Channel` deliberately does
**not**: it enumerates Polis's *own* four ingest channels plus its control
signal, it is not a foreign schema, and a fifth channel is a design decision that
*should* fail to compile until every match site has considered it.

`#[non_exhaustive]` does not restrict matching within the defining crate, so
`polis-events`' own dispatch tables — `EventKind::name`, `ToolKind::parse`,
`BusStats::dropped` and the rest — keep their exhaustive matches and keep the
compile-time guarantee that a new variant was handled *there*. The constraint
lands only on downstream crates, which is the point.

**Consequences.** A `match` on any of the seven outside `polis-events` needs a
wildcard arm, and that arm is where a `ControlEvent::SchemaDrift` belongs rather
than a silent drop. Variants remain constructible downstream — the attribute
restricts matching, not construction — so no call site loses the ability to build
an event. Adding a variant is now a non-breaking change, which is what makes the
append-only tag table of ADR-0044 something the compiler helps enforce instead of
something a reviewer has to remember.

---

## ADR-0049 — `RecordedEvent`: the on-disk format for `polis replay`

**Context.** `EventMeta.observed` is a `std::time::Instant`, and ADR-0014 makes
that load-bearing: order by receipt, never by a timestamp inside a payload,
because 20% of transcript files contain a backwards step and one observed jump
was 60 seconds. A monotonic clock is also immune to NTP corrections and to the
operator changing the system clock mid-session, which PRD §4.3's ±2 s attribution
window depends on.

An `Instant` has no meaning outside its process, and neither `Event` nor
`EventMeta` derived `Serialize`. PRD §15 M2 — "read one JSONL file offline and
animate it over the city", the milestone the PRD says to spend real time on
because most of the visual notation gets decided there — therefore had no on-disk
format at all. Retrofitting a wire format after three milestones of consumers
exist is exactly the change that is cheap today and expensive later.

**Decision.** A new `polis_events::record` module defines the recorded form:

```rust
pub struct RecordedEvent {
    pub wall: WallTime,             // display key
    pub monotonic_offset_ms: u64,   // ordering key
    pub event: Event,
}
```

with `RecordingClock` (live → recorded) and `ReplayClock` (recorded → live), a
`RecordingHeader` carrying `RECORDING_FORMAT`, and JSON Lines as the file format.

Four decisions inside that:

* **Two clocks, one reading.** `RecordingClock` reads the system clock **once**,
  at the recording's origin, and pairs it with the `Instant` taken at the same
  moment. Every event's `wall` is then *derived* as origin + monotonic offset.
  So `wall` and `monotonic_offset_ms` sort identically, and a system-clock jump
  mid-session cannot put a backwards timestamp in the file — the failure mode
  that made transcript timestamps unusable in the first place.
* **`observed` is `#[serde(skip)]`, defaulting to `Instant::now()`.** The live
  path keeps the `Instant`; the recorded path carries time in the two fields
  above; `ReplayClock::live` stamps `observed = mono_origin + offset` so a
  replayed stream is windowable by exactly the same rules as a live one. The cost
  is that serializing a bare `Event` silently loses its timing, which is why
  `RecordedEvent` is documented as the only sanctioned serialization path.
* **`WallTime` is written out**, as `i64` milliseconds since the Unix epoch, with
  no `chrono` or `time` dependency. Same reasoning as ADR-0029: this value is part
  of a persisted format, and a dependency that changes its serialization or its
  calendar handling in a patch release changes the format underneath it. Signed,
  so a rewritten git history's pre-1970 commit is representable — `WallTime` is
  also `polis-repo`'s commit-time type, which is what keeps one timestamp type
  across the workspace.
* **JSON Lines with a version header.** One record per line survives a truncated
  file, matches the transcript idiom Claude Code already uses, and lets a reader
  skip a half-written final line while a recording is still being appended to.
  `RECORDING_FORMAT` exists so a future Polis refuses an unreadable file instead
  of misreading it.

**Consequences.** Eleven types across `polis-events` gained `Serialize` +
`Deserialize`, which makes **the `Event` enum's variant names a wire format**:
serde's external tagging writes `{"Hook":{…}}`, so *adding* a variant is free
(ADR-0048) but *renaming* one invalidates every recording on disk and must bump
`RECORDING_FORMAT`. `polis-ingest`'s replay path and `polis-app`'s
`polis replay` subcommand now have a format to target in M2 rather than
inventing one under deadline.

---

## ADR-0050 — Simplex noise is written out; the `noise` crate is removed

**Context.** `[workspace.dependencies]` pinned `noise = "0.9.0"` for PRD §7.2's
terrain field. PRD §7.4 requires that "the same repo produces the same city on
every launch **and on every machine**", and PRD §16 makes a two-OS byte
comparison of the serialized `CityLayout` "the most important test in the suite".

The terrain field is not decoration. PRD §7.2 makes it the thing every road
contour follows — "its only job is to give roads contours to follow, so curvature
looks justified rather than randomly wiggled". A different noise function is not
a slightly different texture; it is a different road network, therefore different
blocks, lots and buildings. The whole city.

No noise crate promises output stability between releases, and none is under any
obligation to: changing a permutation table or a gradient set is a legitimate
quality improvement for a noise library and a catastrophe for a golden-file
suite. `cargo update` would silently invalidate every stored layout, and the
failure would present as "the determinism test broke on CI" with no obvious
cause.

**Decision.** `noise` is removed from `[workspace.dependencies]` and from
`polis-layout`, and added to the manifest's ABSENT list alongside `wgpu` and
`winit`. 2-D simplex plus fBm are implemented in
`polis_layout::determinism::{simplex2, fbm2, fbm2_gradient}`, with the
permutation table derived from the seed by a written-out function, and pinned by
a test with literal expected values — exactly as `LogicalPath::layout_seed` is
pinned (ADR-0029).

**Consequences.** Roughly eighty lines of well-understood, thoroughly documented
algorithm replace a dependency, and `polis-layout`'s output is now stable against
`cargo update` by construction. If the pinned noise test ever fails, every golden
layout file is invalidated **on purpose** — that is the signal, not a nuisance.
The same rule applies to anything else that might be reached for later: a
Poisson-disc sampler, a Voronoi relaxation, a hash. If its output reaches the
layout, it is written out here.

---

## ADR-0051 — tree-sitter grammar pins look mismatched and are not

**Context.** The runtime is `tree-sitter 0.27.0`; the four grammars PRD §9 needs
resolve to `tree-sitter-rust 0.24.2`, `tree-sitter-javascript 0.25.0`,
`tree-sitter-typescript 0.23.2` and `tree-sitter-python 0.25.0`. That reads like
version skew, and grammar/runtime skew is a real and common build break.

It is not skew here. Since tree-sitter 0.24 a grammar crate depends on the tiny
`tree-sitter-language` ABI shim (0.1.x) and **not** on the runtime; all four
declare `tree-sitter-language = "0.1"` and nothing else. The version number
tracks the *grammar's* releases, not the parser's, and each of the four is the
latest published version.

The real hazard is that a genuine ABI mismatch is a **runtime** failure, not a
compile error: `Parser::set_language` returns `Err(LanguageError)`. PRD §9's
"failure to parse a file is non-fatal — that file simply has no streets" would
swallow it perfectly. Every file would silently have no streets, the streets
layer would render empty, and the map would look entirely healthy.

**Decision.** The four versions above are pinned in `[workspace.dependencies]`,
and `polis-repo/tests/grammar_abi.rs` loads each one into a real `Parser` and
parses a snippet containing the import construct the extractor queries for,
asserting the tree is error-free. All six cases pass against runtime 0.27.0 on
this machine. `.tsx` is modelled as its own `polis_repo::Language` variant
because `LANGUAGE_TSX` is a separate grammar, not a flag on the TypeScript one.

**Consequences.** A grammar bump that crosses an ABI boundary fails one loud test
instead of quietly costing the product its streets. The grammars compile C
through the `cc` crate, which needs a working C toolchain — already required on
Windows by the MSVC target, and the same toolchain `rusqlite`'s `bundled` feature
uses. MSVC's linker prints an informational "creating library …" line for the
grammars' exported symbols, which `rustc` surfaces as a `linker_messages`
warning; the workspace allows that lint rather than living with a permanently
noisy `cargo test` on Windows.

---

## ADR-0052 — Roads are the boundary network of the settled ground

**Context.** PRD §7.2 step 2 specifies space colonisation with intersection
snapping, and warns that "without snapping you get a tree, and trees read as
artificial". The first M1 attempt implemented it literally and got exactly the
warned-about failure, for a reason the PRD does not anticipate: snapping can only
bridge two things that are *already* close. Attractors were scattered per
district on a golden-angle spiral, so each district's cloud was spatially
isolated, snapping never fired across a gap, and the graph came out as one small
tree per island — no cycles, therefore no faces, therefore no blocks, therefore
degenerate lots and degenerate buildings. `docs/city-m1.png` at that revision is
shards in a void.

Three architectures were then prototyped and rendered, scored by an independent
judge on the images and reviewed by the lead (`docs/design/JUDGEMENT.md`).
`accretion` won: simulate the town growing file by file in git commit order, and
take the road network to be the *dual* of the settled ground.

**Decision.** The road network is the boundary network of the settled ground. A
road is the line where one parcel's territory stops and the next one's begins —
the Voronoi diagram of a set of accreted plots, welded, collapsed and pruned. The
pipeline is:

| Stage | Module |
|---|---|
| terrain (PRD §7.2 step 1) | `terrain` |
| district territory (constraint only) | `territory` |
| accretion in commit order (PRD §7.1) | `accrete` |
| cells → welded planar graph | `voronoi`, `roads` |
| blocks = the faces (PRD §7.2 step 3) | `blocks` |
| lots (PRD §7.2 step 4) | `lots` |
| buildings (PRD §7.2 step 5) | `buildings` |

Space colonisation is deleted: `roads::grow`, `RoadGrowth`, `Attractor`,
`scatter_attractors`, `GrowthParams`, `step_towards`, `consume_reached`,
`direction`, `blocks::DistrictSites`, `city::CITY_GROWTH` and
`city::classify_roads` are gone. `RoadGraph`, `Block`, `Lot`, `Building`,
`District`, `StreetLine` and `CityLayout` — the cross-crate contract — are
unchanged, so `polis-render` and `polis-world` were not touched.

PRD §7.2's snap survives as two operations that *do* bridge something: the
**weld**, which fuses corners two adjacent cells computed independently, and the
**collapse**, which contracts every boundary shorter than a threshold and turns
two three-way corners into one four-, five- or six-way junction. At 5 000 files
57 % of junctions are four-way or better; a raw Voronoi diagram is almost all
three-way.

**Consequences.** Planarity, connectivity and closed faces are properties of the
construction rather than of a tuning constant, and there is no parameter setting
at which the graph degenerates into a tree. Measured at 5 000 synthetic files:
one connected component, zero dangling ends, zero crossings without a node, and
1 071 blocks equal to `E − V + C` exactly. The identity is asserted on a fixed
corpus by `eulers_formula_holds_on_a_fixed_corpus`, because a face walk that
drops a face is invisible everywhere else.

PRD §7.2 step 2 is therefore **not implemented as written**, and this ADR is the
record of that. What the PRD is *for* — irregular four- and five-way junctions,
closed blocks, irregularity as the residue of history — is delivered; the
mechanism named in it is not the one that delivers it.

---

## ADR-0053 — The layout is `f64` inside and `f32` at the stage boundaries

**Context.** `polis_layout::Point` is `f32`: it is the cross-crate contract and
what the renderer uploads. The interior of the new pipeline cannot be.

A cell is built by clipping a convex frame with the perpendicular bisector
against each neighbour. Two adjacent cells compute the *same* corner from the
same two bisector equations, but in a different clip order, so the two results
differ by rounding. They become one road junction only if they agree to within
the weld tolerance. The failure mode to design against is **near-cocircular
sites**: four plots almost on a common circle, which accretion produces
constantly because it settles plots at a fixed minimum separation. There the
bisectors meet at a shallow angle and the rounding disagreement is amplified by
orders of magnitude. In `f32` it exceeds the weld tolerance, the corner fails to
weld, and the graph silently loses the cycle the whole design exists to produce —
silently, because a city with one block too few still serializes cleanly and
still looks like a city.

**Decision.** Everything from the accretion search through the half-plane
clipping, the weld, the collapse, the face walk, the lot subdivision and the
footprint fit is `f64`, in `polis_layout::geom` (`Pt = [f64; 2]`). Conversion to
`f32` happens **only** at stage boundaries, through `geom::to_point` /
`geom::to_polygon`, which run `determinism::quantize_f64` first so a value that
reaches the output has already been snapped to the same 0.001 grid the snapshot
prints at. The weld tolerance is 0.004 — coarse enough to absorb the `f64`
disagreement, fine enough that two genuinely different corners stay apart.

**Consequences.** One extra type to convert at four boundaries, and about 30 %
more memory in the working set, neither of which is measurable next to the search
itself. In exchange, the weld is not a source of silent structural loss, and the
quantisation is a single documented step rather than a property of whichever
arithmetic happened to run. Nothing in the crate may take a coordinate from `f32`
into a geometric predicate; if a future stage needs one, it converts through
`geom::from_point` and back.

---

## ADR-0054 — A footprint is the inset lot, not an oriented rectangle

**Context.** PRD §7.2 step 5: "**Buildings** are lots inset by a setback, with a
small random rotation (±4°)." The `accretion` prototype deviated from this,
using an oriented rectangle aligned to the parcel's long axis, on the grounds
that a triangular parcel gives a triangular building and a triangle does not read
as a building.

Measured, that trade is far worse than it looks. The largest rectangle that fits
inside a five-sided Voronoi-derived parcel is under half its area even with a
multi-anchor search, so the rectangle threw away roughly a third of every
footprint in the city. The design bake-off's judge identified building density as
the **cross-cutting** finding across all three entries: footprints were 8.9 % of
block area against 30–60 % for a dense historic core, "the ground is ~90 % empty,
and THIS more than any topology property is why all three renders read as
diagrams rather than cities".

**Decision.** The footprint is the parcel's buildable region, scaled about its
centroid to the target area and rotated ±4°. The buildable region is the parcel
eroded **per edge** (`geom::erode_per_edge`): the full road half-width plus a
kerb on an edge that lies on the block boundary — which *is* a road centre line —
and a garden-fence setback on an interior lot line. Eroding uniformly by the
larger of the two is what made small parcels unbuildable.

**Consequences.** Coverage went from 8.9 % to 31–33 % at 5 000 files and 33 % on
this repository, which is a different category of image. Parcels here come from a
Voronoi cell cut into strips, so they are quadrilaterals and pentagons far more
often than triangles; where a triangle does occur the building is a triangle, and
that is accepted. Every footprint is verified corner-by-corner to be inside its
own lot and clear of the road corridor before it is returned, and shrunk until
both hold — so "no building stands in a road" is enforced, not asserted:
measured zero of 4 565 at 5 000 files, against 79 in the prototype.

---

## ADR-0055 — Footprint area ramps between two fractions of the lot

**Context.** PRD §7.3: "**Footprint area** ∝ `sqrt(file_size_bytes)`, clamped to
`[min_lot, block_area * 0.6]`." Taken as a single absolute constant, that cannot
satisfy a city whose rim parcels are twenty times its core parcels: a constant
tuned for the core overflows nothing and leaves the rim empty, and one tuned for
the rim makes every core building a fleck. The prototype's clamp bound almost
everywhere, so file size stopped being legible at all.

**Decision.** Both halves of PRD §7.3 are kept, as the two bounds they are.
`buildings::fill_fraction` ramps the footprint from `MIN_FILL` to `MAX_FILL` of
the parcel's buildable region as `sqrt(size_bytes)` goes from 512 B to 64 KiB;
the result is then capped above by `FOOTPRINT_SCALE · sqrt(size_bytes)` — the
literal proportionality, which stops a small file on a large outlying plot
getting a warehouse — and by `BLOCK_AREA_FRACTION` of its block, which is PRD
§7.3's own clamp, tightened from 0.6 to 0.40 because a one-file block otherwise
reads as a block with a hole in it.

**Consequences.** A bigger file is always a bigger building, which is the
ordering PRD §7.3 is really asking for, and the density holds at both ends of the
age gradient. The area is no longer *literally* proportional to the square root
of the size across the whole range, and that is the deviation this ADR records.

---

## ADR-0056 — District territory is a constraint; the city limit is the frame

> **Amended by ADR-0058.** The cut orientation below had a defect that made
> the partition hand a group the area computed for the other side of the
> cut, and "there is no contact rule" is now "contact is with a sibling".
> The consequences recorded here are the numbers *before* that fix.

**Context.** Accretion decides district membership emergently — a district is
whatever fell out of where its plots happened to land. Measured at 5 000 files
that gave 102 of 276 districts more than one disconnected piece, so in the dense
core the colour changed every two or three blocks. That violates PRD §9 ("the
tree determines placement") and PRD §8 (the district skeleton must stay readable
at every zoom).

The bake-off's `treemap-arterials` entry had the cure and lost on everything
else. Its recursive tree partition is grafted here **only as a territory
constraint**; its roads are not, and the chord-splitting that produced its
crazed-glaze arterials and its snap-clustered seven-way stars is not present.

**Decision.** `territory` runs a balanced recursive subdivision of a convex city
limit — children ordered by `(oldest file, path)`, split at the index best
balancing quantised subtree weight, face cut at the offset giving each side area
proportional to its weight, cut normal the face's longest axis leaned 30 % toward
the terrain gradient, older group taking the side nearer the civic square. A plot
may then only settle inside its own district's polygon. Roads still come entirely
from the Voronoi of the accreted plots.

Two further decisions follow from measurement and are recorded here because both
reverse the prototype:

* **There is no contact rule.** The prototype required a new plot to be within
  `1.75 × sep` of an existing one, which is what gave it one connected component.
  With a territory partition that rule is redundant *and* harmful: a district
  whose polygon the growth front had not yet reached failed the contact test
  inside its own ground and settled in an ancestor's instead — 83 % of plots at
  5 000 files, which left districts as fragmented as they were with no partition
  at all.
* **There are no phantom sites.** The prototype bounded its outer cells with
  phantom plots laid wherever the ground was empty, and discarded their cells.
  Empty ground *inside* the settlement grows phantoms too, so a gap between two
  quarters became a hole in the map and, when wide enough, split the road graph —
  eleven components at 5 000 files. Every cell is instead clipped to the city
  limit, so the union of the cells **is** the city limit, `components = 1` is
  structural, and the drawn edge of town is a drawn boundary rather than a ragged
  fringe.

A district's ground is sized by `accrete::Params::district_demand`, which counts
`ceil(files / capacity)` whole plots, a slack factor, and a **border band**: a
plot may come no closer than one separation to a plot over the border, so a
district loses about half a separation around its whole perimeter, and that band
is most of a small district's polygon.

**Consequences.** Fragmented districts fell from 102/276 to 41/307 at 5 000
files and to 0/33 on this repository. A plot that still cannot fit inside its own
polygon relaxes, in a fixed order, onto its polygon's fringe, then into an
ancestor's, then anywhere inside the city limit; every rung is counted in
`CityReport` rather than hidden, and the counts are printed by `polis snapshot`.
PRD §7.7's no-rearrangement property is upgraded from measured to structural for
everything inside a district's polygon, since that polygon is fixed by its
parent — but **not** across a repository that has grown, because the partition is
computed from the whole file list and quantised weights make a moved cut rare
rather than impossible.

---

## ADR-0057 — `polis snapshot` renders on the CPU, in the repository

**Context.** PRD §15 M1 ends in "generate a city from git history and render it
to a window (**or PNG**)". `polis snapshot` was declared in the CLI and returned
an error, so the M1 gate had to write its own rendering harness — which means the
thing being tested and the thing being shipped were two different pieces of code.

**Decision.** `polis snapshot` is implemented (`polis-app/src/snapshot.rs`) and
draws through `polis_render::plan`, a static plan renderer over
`polis_render::raster`, a deterministic software rasteriser and PNG writer. It
takes the checkout or a synthetic repository (`--synthetic N`), and writes the
plan, optionally the junction diagram (`--junctions`) and the serialized layout
(`--layout`).

The CPU path is deliberate rather than a stopgap. A PNG has to be producible with
no adapter, no surface and no window; and rasterisation rules differ between GPU
vendors, so two machines would disagree about the bytes of the image while
agreeing perfectly about the layout underneath. The PNG is written with stored
(uncompressed) deflate blocks: a valid zlib stream every decoder accepts, no
dependency, and output that is a pure function of the pixels where a compressor's
heuristics would not be.

The presentation scheme is taken from the bake-off's `voronoi-organic` entry,
which lost on structure and won on presentation: hue families keyed on the
top-level directory, in-map district labels, a drawn city limit, and a metrics
footer. **No timing is ever drawn into the image** — the bake-off found a real
leak there, a wall-clock generation time in the legend that made the PNG
non-reproducible while the layout under it was perfect. Timings go to stdout.

**Consequences.** There is one product path from a repository to a picture, and
the M1 gate asserts that path is reproducible (`the_rendered_plan_is_reproducible`).
Measured: `docs/city-m1.png` and `docs/city-m1-large.png` are byte-identical
across separate processes and across the debug and release profiles. The
interactive wgpu renderer is untouched; this is a second, offline output, not a
replacement.

---

## ADR-0058 — Two rules make a district one piece, and the second is the load-bearing one

**Context.** ADR-0056 grafted `treemap-arterials`' recursive partition onto the
accretion layout as a territory constraint, and it did not work: fragmented
districts only fell from 102 of 276 to 41 of 307 at 5 000 files, six of the
thirteen top-level packages were still in more than one piece, and **636 of
1 198 plots settled outside their own district's polygon**. The recorded
explanation was that a small district's polygon is mostly exclusion band.

That explanation was wrong, and the measurement that found it out is worth
recording. Ranked by *ground given over ground needed*, the worst districts were
not the small ones: `web/adapters/server/retry/policy/hasher`, 769 files, was
given a polygon of **0.03 square units against the 494 its plots needed**, and
every one of its 129 plots relaxed into an ancestor's territory. 146 of 279
districts had less ground than their own plots occupied.

The cause was one inverted assignment in `territory::place`. A cut has two jobs:
give each side area in proportion to its group's quantised weight, and put the
older group on the side nearer the civic square (PRD §7.1). The code solved the
offset for the first, then chose which *half* to hand to which group by the
second — so whenever the younger group's centroid was nearer the origin, each
group received the area computed for the other. On a balanced cut that is
invisible. On an unbalanced one it is catastrophic, and the deeper the tree the
more often it compounds.

**Decision.** Three changes, in the order they matter.

1. **The cut's normal is oriented before the offset is solved for**, so that the
   half with the area the first group asked for *is* the half nearer the civic
   square. One constraint is met by choosing the sign of a normal and the other
   by the offset, and neither has to be traded for the other. `districts`'
   `a_district_that_asks_for_more_ground_gets_more_ground` states the property as
   a monotonicity over every pair of districts, so it holds at every depth.

2. **Rule A: after a district's first plot, every plot's nearest plot in the
   whole city must be one of its own district's.** Rule T (a plot settles only
   inside its own polygon) is not enough on its own — a convex polygon says
   nothing about cells, and a neighbour's cell can reach through it. The
   nearest-neighbour graph is a subgraph of the Delaunay graph, so rule A makes
   each new cell share an edge with a sibling's, and a district's cells are one
   edge-connected region by induction over the growth order. This reverses
   ADR-0056's "there is no contact rule": the prototype's rule required contact
   with *any* plot, which is what fought the partition; contact with a *sibling*
   is always available and serves it.

   The ladder drops rule T before rule A — a plot may sit up to `0.55 × sep` over
   its own border if that is what lets it keep contact with its own ground —
   because a plot half a separation over the line is still in its own quarter,
   while a plot out of contact is a second piece of the district.

3. **A maximum spanning forest of each district's cell-adjacency graph is never
   contracted** (`districts::links_to_keep`, `roads::Graph::collapse_short`).
   Contracting a boundary is what the collapse pass is *for* — it is where the
   four- and five-way junctions come from — but it leaves the two cells either
   side touching at a node, and when that boundary was the only thing joining a
   two-parcel district, the district comes out in two pieces. Measured: one
   district in 238 at 3 000 files. Taking the **longest** candidate at each step
   means the protected boundaries are the ones a short-edge pass would never have
   touched, so the cost is 0.5 points of four-and-five-plus junction share.

**What was tried and rejected.** A fourth change would have made "adjacent
directories are adjacent on the ground" structural at *every* level of the tree
rather than only at the leaves: require a district's **founding** plot to have a
plot of its nearest already-settled ancestor quarter as its nearest neighbour, so
that a new quarter buds onto the quarter it belongs to. The induction is sound —
it makes every subtree connected, not just every district — and the measurement
says no. At 1 000 files it took fragmented districts from 0 to 1, fragmented
packages from 0 to 1, and rule-A bends from 0 to 18, because a founding plot is
the one with the least room to manoeuvre and forcing its neighbour pushes it
somewhere worse. The weaker property is kept and *measured* instead:
`CityReport::fragmented_subtrees` is 0, 1, 4 and 2 of 32, 144, 238 and 312
subtrees at 200, 1 000, 3 000 and 5 000 files, and the gate bounds it at one in
twenty rather than pretending it is zero.

**Consequences.** Measured at 5 000 files against the same corpus, the same
seed and the same `lots`/`buildings` code on both sides, so the numbers are this
change and nothing else.

| metric | before | after |
|---|---|---|
| fragmented districts | 41 of 307 | **0 of 312** |
| fragmented top-level packages | 6 of 13 | **0** |
| plots settling outside their own polygon | 712 of 1 198 | **0** |
| districts with no ground of their own on the map | 5 | **0** |
| faces the partition could not divide | 23 | **0** |
| road nodes / segments | 1 262 / 2 332 | 1 299 / 2 354 |
| components / crossings / dangling | 1 / 0 / 0 | 1 / 0 / 0 |
| blocks (= independent cycles) | 1 071 | 1 056 |
| four-and-five-plus share of junctions | 57.1 % | 52.5 % |
| longest natural stroke | 76.4 % of diameter | 76.1 % |
| block p95:p05 | 20.2x | 18.0x |
| median block compactness | 0.718 | 0.707 |
| age gradient (rim block area / core) | 2.98x | 4.00x |
| building coverage of block area | 31.4 % | 34.0 % |
| full generation | 2 276 ms | **275 ms** |

Zero fragmented districts and zero fragmented packages also at 200, 1 000 and
3 000 files and on both pinned fixtures, where the baseline was 2 of 31, 11 of
144 and 41 of 307.

The junction share is the price, and it is stated rather than buried: 4.6 points,
of which about half is the protected spanning forest and half the adjacency rule.
It stays above the 45 % floor the gate holds and close to the bake-off's 53.3 %
baseline. The generation time falls by a factor of eight because the relaxation
ladder is no longer walked 712 times.

`CityReport` gains `settled_nonadjacent` (rule A bent), `presplit_districts` (a
district the Voronoi diagram itself never joined, which no later repair could
fix) and `fragmented_subtrees`; the first two are 0 at every scale measured and
the M1 gate asserts it rather than printing it. Contiguity is not a tuning target
any more: any fragmented district at all now means one of the three mechanisms
above failed.

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

## ADR-0059 — Through-streets come from a **fan of quarters**, not from seeding the plots

**Status** accepted · **Supersedes nothing; completes ADR-0052's road model.**

The design bake-off's second defect was that the longest natural road stroke was
28 % of the city diameter with exactly one stroke past 25 % — the "soap-foam
honeycomb" tell. Its remedy, Graft 2, was to promote the district territory
boundaries to *desire lines* and pull the plots near one onto a lattice aligned
to it: a run of collinearly-seeded sites gives a run of collinear Voronoi
boundaries, so the straightness would be emergent from the seeding rather than
drawn.

**That was built first, and measured.** It does not reach at this plot density,
and the reason is arithmetic rather than tuning. A boundary segment on the line
needs a *facing pair* — two plots mirrored across it — and the two sides of a cut
are different districts settled at different times, so a run needs the second
district to choose exactly the rungs the first one used, over and over. The share
of lattice slots that can be filled at all is `pitch² × plot density`, about a
quarter at 5 000 files. Measured, with the slots reserved from before the first
file was placed and a bonus for completing a pair: **235 plots exactly on a
lattice, 44 facing pairs, longest run of consecutive filled rungs = 2**, against
the eighteen cells a 46 %-of-diameter avenue spans. Sweeping the lattice pitch
from 1.0 to 4.0 × `sep` moves the longest run between 1 and 5 and never further.

So the avenue is made **structural**, in the one way that does not draw a road:
the diagram is computed *per quarter*. `territory` fans the city limit into
`WEDGES` wedges round the civic square; a plot's cell starts from its own
quarter's polygon and is clipped only against plots of the same quarter. Each
quarter is tiled exactly by its own cells, the quarters tile the limit, and the
shared edge between two quarters is the chord itself — one exactly straight road,
for its whole length, with no coordination between the two sides.

Why this is not treemap's chord-splitting, which the bake-off rejected:

* treemap made **every** cut at every depth a road; here it is the fan's rays,
  the civic square's sides and one ring road per wedge — at most 27 lines against
  some three hundred cuts, and every other road in the city is still the bisector
  of two accreted plots;
* treemap's arterials were **city-spanning**; a ray runs from the civic square to
  the limit and stops, which is 46 % of the diameter, and the acceptance band's
  upper bound exists precisely to forbid the other thing.

The fan, rather than a binary chord split, is what makes the second bullet
possible: *any* binary split of a convex region starts with a full chord of it,
so the first quarter boundary of a chord partition necessarily runs clean across
the city.

The lattice seeding is kept. It no longer makes the avenues straight; it makes
the frontage along them regular and the cross streets meet them squarely.

Measured at 5 000 files, before → after: longest street stroke 41.8 % → 45.8 % of
the city diameter, strokes past a quarter of it 13 → 23, and — the number that
actually separates an avenue from a wiggle that happens to end far away — the
median sinuosity (arc length over end-to-end reach) of the ten longest strokes
1.07 → 1.00.

## ADR-0060 — The stroke measurement excludes the city limit, and uses the diameter

**Status** accepted

Two bugs in the number the bake-off set a range for, both of which flattered it.

1. **The city limit was counted as a street.** Every cell is clipped to the
   limit, so the limit is in the road graph as a chain of collinear edges broken
   only at the limit polygon's own corners — and a nineteen-sided polygon turns
   18.9° at a corner, inside the 40° continuation limit. The stroke rule walked
   three quarters of the way round the outline and reported it as the city's
   longest through-street: **74 % of the diameter on every seed tried**, before
   and after any change to the plan. It is the outline every time. A chain that
   uses a perimeter edge is no longer counted; `polis snapshot` prints both
   numbers so the exclusion is visible rather than assumed.
2. **The denominator was the bounding box's diagonal.** For a roughly round city
   that is `2R√2` against a true diameter of `2R`, so every stroke read a factor
   of √2 short — the difference between a 32 % avenue and a 46 % one.

Also: the continuation limit itself was `cos 56.6°`, not the conventional 40°.
On one graph, the same morning: 76.1 % at 56.6°, 75.6 % at 40°, 46.4 % at 30°,
and 12, 5 and 4 strokes past a quarter of the city. A measurement that flatters
the thing it measures is worse than no measurement, so it is pinned at the
convention.

## ADR-0061 — The weld is a tolerance, not a bucket

**Status** accepted · **Fixes a latent planarity break in ADR-0052's weld.**

Cell corners were welded by rounding onto a `0.004` lattice and grouping equal
keys. That is a *bucketing*, not a tolerance: two corners a thousandth apart weld
when they land in the same bucket and do not when the bucket boundary happens to
fall between them. An unwelded pair leaves two nodes a thousandth apart with four
edges between them, two of which cross without a node — the exact planarity break
PRD §7.2 forbids, produced by a weld that was *asked* to fuse them and silently
did not.

It stayed latent while the plots were irregular and appeared the day the desire
lines started seeding plots on a lattice, because a lattice is precisely what
puts corners near a bucket boundary over and over. The lattice now finds
*candidates* and a second pass unions neighbouring buckets whose corners really
are within the tolerance, in sorted key order so the grouping is a property of
the geometry rather than of the order the cells arrived in (PRD §7.4).

## ADR-0062 — Streets are drawn on the roads, and are a layer that is off by default

**Status** accepted

PRD §9's streets are cross-district import relations. They were drawn as
centroid-to-centroid chords slashing across open ground, which the bake-off
called the single most anti-city element in the frame. They are now routed by
Dijkstra over the road graph with arterials discounted, so a street prefers a
main road exactly as traffic does and no street is a chord; width is the number
of distinct import edges the relation carries, as PRD §9 asks; and the layer is
**off at the widest zoom** (`polis snapshot --streets` turns it on), because the
whole import graph drawn over the whole city at once is noise rather than
information. §9's free diagnostics — a district with streets to everywhere is a
hub or a god-module, one with none is isolated — are counted into
`city::street_diagnostics` and printed with the metric set.

## ADR-0063 — The block size hierarchy stops at 20×, and the reason is the directory tree

**Status** accepted

The bake-off's fifth defect asks for a `p95 : p05` block area ratio of `>= 30×`
against a prototype's 6.8×. It is 19.6× at 5 000 files, 24.5× at 3 000 and 29.0×
on this repository. Three ways of moving the last of it were built and measured:

* a hotter prune (`PRUNE_RIM` 0.42 → 0.68) gives 21.2×: an independent coin per
  boundary cannot make a heavy tail whatever its bias;
* a block size cap that ramps outward, merging faces while their combined area is
  under a local target — the shape of rule that *does* make a heavy tail — gives
  20.4× at its best setting, and was removed;
* coarser parcels reach the target and cost the map: `sep_rim` 2.75 → 3.90 gives
  28.1× at 27.5 % building coverage, `sep_core` 0.55 → 0.36 gives 30.4× at 28.8 %
  and fourteen slivers, both against 19.6× at 36.0 %. Coverage is the judge's
  cross-cutting finding and the most important number on the map.

The binding constraint is none of these. A district border is never pruned, so a
block can never grow past its own district's ground, and at 5 000 files there are
312 districts over 1 077 blocks — three and a half blocks each. The hierarchy is
capped by the directory tree's own granularity. The honest ways past it are a
coarser rim at the cost of coverage, or letting blocks merge across a district
border at the cost of the contiguity the whole territory graft exists to
guarantee. Neither is worth 10×.
## ADR-0064 — PRD §7.1's age ramp reads **commit time**, not growth-sequence position

**Status** accepted · **Supersedes the ramp inside ADR-0052's accretion.**
Implemented in `polis_layout::age`.

Every grain decision in the city — plot separation, plot capacity, the prune
probability, and through them block size and building coverage — is driven by one
number, `t ∈ [0, 1]`, the *age of the ground*. Until now that number was a file's
**index in the growth sequence divided by the file count**. That is a rank, not a
time, and it silently asserts that a repository adds files at a constant rate.
PRD §7.1 says something quite different:

> Files added in the repo's **first year** form the old town — dense, tangled,
> irregular. Files added last month sit on the periphery.

Two very common repositories break the rank ramp in opposite directions, and both
were measured rather than imagined:

* **Django** — 7 083 surviving files over 21.1 years, of which **235 (3.4 %)**
  were added in the first year. On the rank ramp those files owned 3.4 % of the
  grain range: no old town at all, in a repository that plainly has one.
* **Neovim** — founded in 2014 by importing Vim's tree wholesale, 380 files in
  the first commit and 1 412 (36.3 %) inside the first year. The rank ramp spread
  those 380 *identically aged* files smoothly across a third of the range and
  drew a gradient that does not exist.

The ramp is now a piecewise-linear function of the commit timestamps
`polis_repo::git` already carries in `FileMeta::added_at`. The repository's first
year owns the first `OLD_TOWN_BAND` (0.40) of the range whatever share of the
files it holds; everything after spreads over the rest by real elapsed time. The
formula is continuous in the history span, so there is no cliff at the one-year
mark.

**What each degenerate history degrades to**, which is the part that had to be
decided rather than discovered:

| History | Kind | Result |
|---|---|---|
| ≥ 1 year | `Calibrated` | PRD §7.1 literally. Measured age gradient (rim block area ÷ core): Neovim **5.96×**, Django **1.65×**. |
| 0 < span < 1 year | `Relative` | The whole repository is inside its own first year. The absolute ramp would compress every file into the first few percent, so the observed range is stretched to at least `MIN_SPREAD` (0.35) and the reading becomes relative: "oldest in *this* repository", not "older than a year". A month-old repository is a dense town with a small fringe, not a metropolis. |
| one commit, or a squash import with nothing since | `Uniform` | There is no age information, so **none is drawn**: one grain everywhere, `NO_HISTORY` (0.5). Measured age gradient **0.96×**. The tempting fallback — rank position — would invent a gradient out of `git log`'s within-commit path order, and the operator would read alphabetical order as history. |
| a decade with a wholesale import at the root | `Calibrated` | The imported files really are all the same age, so they all sit at `t = 0` and the old town is correspondingly large and fine. That is the truth about Neovim and it is what makes an imported tree look imported. |

Three determinism properties, all required by PRD §7.4: no clock is read (`first`
and `last` come from git); the table applies a running maximum over the growth
sequence, so a commit date that goes backwards across a merge cannot make "lowest
growth index" stop meaning "oldest ground"; and every value is quantised to
`1/4096` before it leaves the module, so the `powf` in `sep_at` sees a small
stable set of inputs and a one-second difference in a commit date cannot move a
building.

The ramp is **frozen at generation**. An incrementally added file reads at the
newest calibrated value; recalibrating would move `last`, which moves every
file's `t`, which moves every plot — exactly the ground-moving PRD §7.7 forbids.

*Consequence for the gate.* The block size hierarchy and the age gradient are now
properties of the *repository* rather than of the generator. Measured with one
build: Neovim (36.3 % first-year) 23.8× and 5.96×; the synthetic fixture (8.2 %)
10.0× and 2.07×; Django (3.4 %) 5.2× and 1.65×. The previous 20.2× and 2.98×
were numbers the rank ramp manufactured for every repository alike. The M1 gate's
bounds were lowered to sit below all three, and
`the_age_ramp_follows_real_commit_time` — four histories over one unchanged file
list, which must produce four different cities — is where the ramp's *response*
is asserted instead.

## ADR-0065 — Polis does not ingest its own output: the repo-walk exclusion

**Status** accepted · Implemented in `polis_repo::tree::WalkExclusions`.

The city-layout design bake-off rendered its candidate cities into
`docs/design/`. `docs/design/` is inside the repository. So each run laid out a
repository that the previous run had made larger: the town grew a few files by
itself between two runs that were supposed to be identical. Nothing errored, no
test failed, and the only symptom was a layout that drifted — the single hardest
class of bug to diagnose in a system whose entire product promise (PRD §7.4) is
that the map does not move. It was live in this repository when it was found.

It is a feedback loop, not a classification problem, and it is closed by refusing
to walk what Polis writes.

**Why this is not the industrial list.** The two look alike and mean opposite
things. PRD §8 requires `node_modules` to be *drawn*, as one dull mass — it is
part of the repository and the operator should see how much of it there is. An
excluded path is not drawn at all, because it is not part of the repository in
any sense the operator cares about: it exists because Polis ran. Merging the
lists would either start drawing the tool's own PNGs as buildings or stop drawing
the dependency tree PRD §8 asks for.

**Configurable, and that is not optional.** A hardcoded list is wrong in both
directions. Every project puts its generated artefacts somewhere different and
the shipped list will always be missing one; and a repository whose `docs/design`
is hand-written prose would have a real district silently vanish with no way to
say otherwise. So there are two rules — a directory name at any depth, and a
root-anchored path — plus `remove_dir` for the opposite case.

The shipped default is deliberately two entries (`docs/design`, `.polis`),
because the general closure is not a list at all: it is
`WalkExclusions::exclude_output`, which every writer of a file into the checkout
calls with the path it is about to write. `polis snapshot` excludes `--out`,
`--junctions` and `--layout` before it walks. **By name, not by directory**:
`--out docs/city.png` must cost one building, not the `docs/` district.

The regression test is a *sequence* — walk, write a render into an excluded
directory, walk again, assert byte-identical — because a feedback loop between
consecutive runs is invisible to two independent runs. It is paired with the
inverse (`WalkExclusions::empty()` must see the new file), so it cannot pass by
the walk having stopped seeing anything, and with an end-to-end leg in the M1
gate that compares the whole city's digest on a real checkout with real history.

## ADR-0066 — A quarter may only be subdivided by the cut that owns the whole of it

**Status** accepted · **Fixes a silent 55 %-of-the-city failure in ADR-0059.**

ADR-0059 computes the Voronoi diagram *per quarter*: a plot's cell starts from
its own quarter's polygon and is clipped only against plots of the same quarter,
which is what makes the shared edge between two quarters an exactly straight road
for its whole length. The construction rests on one invariant the code did not
enforce: **the leaf quarters must tile the city limit.**

Promoting a cut to a quarter boundary marks the parent quarter non-leaf and
pushes its two halves as new leaves. That is correct when the cut owns the whole
quarter — which it does for every wedge the fan produces. It is wrong when the
root fan does *not* commit, because the recursion then reaches depth 1 twice with
the same quarter, and if the first cut is promoted and the second is not, the
first retires a frame the second half of the city is still using.

Django takes exactly that path (its root fan does not commit) and the result was
invisible in every number the report printed: **two** leaf quarters covering less
than half the map, 1 453 of 2 413 plots clipped against a polygon they are not
inside and coming out with an **empty cell**, 1 483 plots landing inside no face
and attached by `BlockIndex::locate`'s nearest-centroid fallback to whichever
block happened to be closest — one sliver received 517 files — and **2 006 of
7 014 files sharing a parcel** four stages downstream. Every count in the report
was accurate and none of them said so.

Three changes, in the order they matter:

1. **The rule.** A `Quarter` now carries whether the face it describes *is* the
   quarter or only part of it, and only a face that owns the whole quarter may
   subdivide it.
2. **The net.** `leaf_quarters` checks that the leaves' areas add up to the city
   limit's and falls back to the whole limit as one frame when they do not. It
   is an area test, which catches a gap but not an overlap; that is the right
   trade, because the quarters are exact half-plane clips of one convex ring, so
   a gap is what a bookkeeping slip produces and an overlap is not.
3. **The instruments.** `CityReport` gained `empty_cells` and `plots_off_face`,
   both zero by construction and both asserted at zero in the M1 gate. The bug
   was not that the fallback existed — a fallback is right — but that it was
   *unbounded and uncounted*.

Measured on Django, before → after: empty cells 1 453 → **0**, plots off their
face 1 483 → **0**, files sharing a parcel 2 006 → **0**, buildings 4 988 →
**7 011 of 7 014**, blocks 897 → 2 346.

This is the finding that justifies the judge's instruction to run a real
repository *before* the gate rather than after. Three synthetic corpora, four
scales each, and both pinned fixtures all take the fanned path and none of them
reaches the branch.

## ADR-0067 — The absolute footprint cap is measured against the repository's own median file

**Status** accepted · **Amends ADR-0055.**

`FOOTPRINT_SCALE · √bytes` is an area in **world units**, and the world's scale
comes from the plot spacing, which comes from the file *count*. So the cap
silently encoded an assumption about the average file, and it binds or does not
bind according to how a repository's typical file compares with the corpus it was
calibrated on.

Measured on two real repositories through one build:

| repository | files | median file | median lot fill | coverage |
|---|---|---|---|---|
| Neovim | 3 890 | 7.7 kB | 58.6 % | 29.1 % |
| Django | 7 014 | 1.9 kB | 26.0 % | 15.8 % |

Django is not a sparser repository than Neovim. It is a repository of small
Python files, and the cap turned that into a city of specks on a wire mesh —
the judge's cross-cutting finding ("not enough building") returning on the first
corpus nobody had generated. It is calibration rather than architecture, exactly
as the judge said.

The byte term is now measured against the repository's own median file size:
`FOOTPRINT_SCALE · √bytes · √(6 kB / median)`, clamped to 2.5× either way so the
cap still exists for a repository of stubs. A file twice the median gets √2 times
the cap in every repository; the *ordering* PRD §7.3 asks for is untouched,
because the normalisation is one factor shared by every building in the city. The
median is taken over the files that will actually carry a building — PRD §8's
massed trees are excluded, so a `node_modules` full of minified bundles cannot
set the scale of the source city — and it is the lower of two middle elements,
never their average, so it is an integer that cannot differ in the last bit
between targets (PRD §7.4).

Measured after: Django coverage 15.8 % → **21.1 %** and median lot fill 26.0 % →
**31.6 %**; Neovim 29.1 % → **30.4 %**; the 5 000-file fixture 31.4 % → **32.2 %**.
Django is still the lowest and still short of the 30 % a dense historic core
runs; the remaining lever is `buildings::grain_share`, which deliberately thins
the fill on coarse plots to carry the age gradient, and Django's ground is almost
all coarse.

## ADR-0068 — Seating buildings is parallel; nothing else in the pipeline is

**Status** accepted

`City::accrete` re-runs the whole assembly after one growth step, so PRD §13.1's
50 ms incremental budget is spent on `city::assemble`. Timed at 5 000 files:
weld 0.003 ms, links 2.5, collapse 2.8, faces 2.9, prune 1.4, betweenness 1.1,
blocks 2.6, **lots 12.4**, **buildings 30.4**, districts 1.4, measure 1.3 — a
58 ms assembly of which the building stage is more than half, and the measured
p95 for a single add was 66 ms, over budget.

Seating a building is the only stage that is a pure function of one parcel: the
footprint comes from the parcel ring, the block ring around it and a seed hashed
from the file's own path, and it reads nothing another parcel writes. Everything
upstream is a graph the next step mutates. So that one stage is a `rayon`
`par_iter().map(…).collect::<Vec<_>>()`, which is the exact shape rule 4 of
`polis-layout`'s module documentation permits: results are collected **in index
order** and only then folded sequentially; no thread writes to a shared map and
nothing is pushed as it finishes.

Buildings 30.4 ms → **5.9 ms**, single-add p95 66.3 ms → **38.9 ms**, and the
city's digest is byte-identical before and after — `41f20fff6e1c4516` either way,
which is the only evidence that matters for PRD §7.4.
## ADR-0069 — The derived history is **one** `git log` pass, and it is cached

**Status** accepted · **Amends the two-pass rule inside `polis_repo::git::History::read`.**

PRD §13.1 budgets cold start to first frame at under 3 s for a 5 000-file
repository. Measured before this change, `polis snapshot` end to end: Neovim
(3 890 files, 12.6 years, 37 934 commits) **2.77 s**, Django (7 014 files, 21.1
years, 34 898 commits) **3.13 s** — over budget on a repository the product will
routinely be pointed at, and 90 % of it on the one it was sized for.

Almost all of it was subprocess I/O, and the phase breakdown says so — which is
the first thing this change added, because the budget had been blown by the one
stage nobody was timing:

| stage | Neovim | Django |
|---|---|---|
| walk the checkout | 9 ms | 91 ms |
| **`git log` history** | **1 900 ms** | **1 900 ms** |
| tree-sitter imports | 15 ms | 412 ms |
| `git diff --numstat` | 535 ms | 225 ms |
| layout | 272 ms | 421 ms |
| render + PNG | 38 ms | 79 ms |

Two decisions, both reversing a previously stated one.

**One pass, not two.** `History::read` ran PRD §7.1's pinned
`git log --diff-filter=A --name-only` for the growth order and a second,
unfiltered `git log --name-only` for PRD §8's last-touched times, on the
principle that the city's most load-bearing input should have exactly one
derivation. Measured, that principle costs a *whole extra traversal of the
history*: one full walk is 0.90 s on Django and 0.95 s on Neovim. It is also
avoidable at no cost to the principle, because `--diff-filter=A` selects entries
whose status letter is `A` and `--name-status` **prints that letter** — the two
derivations are one selection written twice. `History::read` is now a single
`--name-status` walk; `GrowthSequence::bootstrap` remains the PRD's pinned
command, and `the_fused_walk_agrees_with_the_two_pinned_commands` asserts, over
four shapes of history including a re-added file and non-ASCII paths, that the
fused walk reproduces both of them **exactly**. The objection was "two code paths
that must agree forever"; the answer is a test that they do, not a second walk.

**Both halves are cached, not one.** PRD §7.1 says "cache the derived growth
sequence keyed on `HEAD`; recompute incrementally on new commits", and only the
growth sequence was, on the reasoning that `last_touched` is broadly invalidated
by any new commit. True, and beside the point: the launch that follows *no* new
commit is the common one, and it was paying a full history walk for a map that
had not changed. The cache is now the whole `History`, versioned, and every
failure — absent, unreadable, truncated, foreign version, different `HEAD` — is a
**miss**, never an error.

It lives at `%LOCALAPPDATA%\polis\history\<key>.json` (`$XDG_STATE_HOME`
elsewhere), beside the corpus store and deliberately **outside the checkout**: a
cache written into the repository would be walked, given a building, and change
the city, which is the feedback loop ADR-0065 exists to close. `key` is the
written-out FNV of the normalised root, the same folding `worktree_id_for` uses,
so two worktrees get two caches and `C:\Repo` and `c:/repo/` get one.
`last_touched` is stored as an **array of pairs** rather than a JSON object: a
map keyed on `LogicalPath` would make a path containing a character JSON escapes
into a round-trip that depends on the encoder.

Measured after, wall clock, same machine:

| | Neovim | Django |
|---|---|---|
| before | 2.77 s | 3.13 s |
| first launch ever, nothing cached | 2.58 s | 4.43 s |
| **every launch after** | **0.99 s** | **1.86 s** |

Django's first-ever launch is still over budget, and that is reported rather than
hidden: it is a 7 014-file repository with 34 898 commits over 21 years, half
again the size PRD §13.1 names, and 1.65 s of its 4.43 s is git reading 21 years
of trees off a cold disk. Every launch after it is 1.86 s. The remaining lever on
the first one is tree-sitter (1.5 s on Django, cold), which is a second cache and
a different decision.

## ADR-0070 — Where the cold-start time actually goes is now printed

**Status** accepted · Implemented in `polis_app::snapshot::Phases`.

A one-line consequence of ADR-0069 worth recording on its own. `polis snapshot`
used to print `TIMING full generation = 411 ms (budget 3000 ms)` and look
comfortable while the process took 3.13 s, because it timed the layout and
nothing else. It now prints every stage between process start and a frame —
walk, history, imports, diff, layout, render — and their total against the
budget.

The general rule: **a budget line that measures one stage of six is worse than no
budget line**, because it converts "we are not measuring this" into "we measured
it and we are fine". The 1.9 s that blew PRD §13.1 was in a stage the report did
not have a column for.

## ADR-0071 — A growth step remembers what it worked out; it does not recompute the city

**Status** accepted · Implemented in the new `polis_layout::memo`.

PRD §13.1 budgets an incremental layout step at under 50 ms off-thread, and PRD
§7.4 requires that "growth is genuinely incremental — a new file runs one growth
step, it does not regenerate the world". Measured at 5 000 files before this
change: a single add moved a **median of four of 1 165** road nodes and took
**44 ms** on twenty-four threads — and **61 ms on one**, which is the number that
matters, because a CI runner has two cores and the budget was already flaking at
52.6 ms under contention.

The step was recomputing essentially the whole city to record a change to a
thousandth of it. `City::accrete` re-runs `city::assemble`, and at 5 000 files
that is 11 ms re-cutting 866 blocks byte-identical to the previous step's and
30 ms of CPU re-seating 5 474 buildings on parcels that had not moved.

**Both stages are pure functions**, which is the property `assemble` already
relied on to run the building stage under `rayon`. The same property makes their
results *reusable*, and reuse is the better use of it: parallelism hides the cost
on a twenty-four-core developer machine and returns it in full on a two-core
runner, whereas work that is not done is not done anywhere.

Three things make this safe rather than a source of stale geometry:

1. **Every hit is verified exactly.** A 64-bit digest picks the bucket; the
   stored arguments are then compared *bit for bit* (`f64::to_bits`, never `==`,
   so `-0.0` cannot match `0.0`). A digest collision costs a recomputation and
   can never produce a wrong building. Nothing here can weaken PRD §7.4 — a cache
   that cannot change an output can only skip producing one already known.
2. **`City::forget` and a test that uses it.**
   `a_warm_cache_and_a_cold_one_build_the_same_city` grows two cities by the same
   six files, forcing one to forget everything before each step, and asserts the
   digests are identical at every step. A key that stops covering one of the
   seating function's inputs fails there, on the first run.
3. **The reuse is asserted, not just reported.** A growth step that recomputes
   the whole city can still come in under 50 ms on a fast machine and is still
   the bug. `incremental_budget.rs` fails below 50 % cut reuse or 75 % building
   reuse.

**The lot id is a label, not geometry.** `place_in_parcel` takes a `LotId`,
stores it on the building, and never reads it. Keying on it looked correct and
was catastrophic: one new plot adds a parcel and **every lot after it
renumbers**, so a content-addressed cache that included the id missed on ~40 % of
the city for a label. It is excluded from the key and stamped onto the remembered
footprint instead — measured, that alone took building reuse from ~60 % to 87 %.

**The lookup structure was measured, not assumed.** Three were tried against the
4.4 ms (parallel) / 30 ms (sequential) of seating they exist to skip:

| structure | hit rate | overhead per assembly |
|---|---|---|
| `Vec` indexed by `LotId` | ~60 % | ~0, and useless for the renumbering above |
| `BTreeMap` on the digest | 99 % | **~6 ms** — a 5 474-node tree allocated fresh every step, more than the seating it replaced |
| a written-out flat probe table | 99 % | ~1 ms |

The table is written out rather than reached for from `std` because a `HashMap`
in this crate is forbidden and `m1_gate::no_hash_map_reaches_the_layout` asserts
that structurally against the source. This table is never iterated, so its order
could not reach an output in any case — but "it would have been fine here" is not
a rule anyone can check, and twenty lines is cheaper than an exception.

**The quarter assignment is carried, not recomputed.** `City::accrete` re-ran
point-location for all 5 000 plots against all ~14 quarters on every added file.
The plots have not moved and the quarters have not changed, so the assignment is
carried on `Growth` and extended by the new plot alone, by the same rule;
`the_carried_quarter_assignment_matches_a_fresh_one` holds the two together.

Measured after, release, twelve single-file adds at 5 000 files:

| | median | p95 |
|---|---|---|
| before, 24 threads | 44.1 ms | 46.5 ms |
| before, 1 thread | 61.2 ms | 71.9 ms |
| after, 24 threads | **37.7 ms** | **43.3 ms** |
| after, 4 threads | 36.2 ms | 44.9 ms |
| after, 1 thread | **36.0 ms** | 44.7 ms |

The result to read is not the fastest row: it is that **24 threads and 1 thread
now give the same answer**. The old step was 39 % slower without cores to hide
in; the new one does not care.

**What is still recomputed, and what it would take not to.** The road graph is
still rebuilt from the whole cell set — weld, collapse, face walk, prune,
betweenness and block assignment are 14 ms of the 36. Localising that needs a
**stable block identity**, and the pipeline does not have one: `lots`'s cut seed
is mixed from the block's *index*, so inserting a single face legitimately
re-cuts every block after it. That is exactly why cut reuse is 71 % where
building reuse is 87 %, it is the next thing to fix here, and it is a layout
change rather than a caching one.

## ADR-0072 — A budget is measured with the machine to itself

**Status** accepted · Implemented as `polis-layout/tests/incremental_budget.rs`.

The incremental-budget assertion lived in `m1_gate.rs` with twenty-two other
tests, and `cargo test` runs the tests inside one binary **in parallel**. Several
of those tests generate a five-thousand-file city. So the growth step was being
timed on a machine every core of which was already saturated by the harness doing
the timing. One build, one machine, release:

| how it was run | median | p95 |
|---|---|---|
| alone | 36.0 ms | 44.7 ms |
| racing the rest of the gate binary | 42.3 ms | 58.8 ms |

Same code. The second row is a measurement of the test harness, and it is where
the reported 52.6 ms came from. A budget assertion whose value depends on how
many other tests happen to be running is a flake, and on a two-core runner a
permanent one.

Cargo runs test **targets** sequentially, so a target holding a single test gets
the machine to itself. That is the whole reason the file exists, and it is the
reason it may not grow a second test.

**This is not the budget being loosened**, and the distinction matters because
loosening it was the available shortcut. The asserted number is PRD §13.1's
50 ms, unchanged; ADR-0071 removed 40 % of the work; what changed here is that
the step is measured rather than the harness. Both numbers are reported —
`POLIS_INCREMENTAL` carries median, p95, moved-node median and both reuse rates,
and `POLIS_INCREMENTAL_EACH` carries all twelve, because a p95 over twelve
samples is the maximum and a single outlier is worth seeing rather than
summarising.

Measured under deliberate external load for the record, and **not** asserted: on
a half-loaded machine the step is 39–45 ms median and 48–62 ms p95; on a fully
saturated one, 68–78 ms median and 155–172 ms p95. No algorithm meets a
wall-clock budget on a machine that is not there to run it, and pretending
otherwise is how a budget becomes noise.

## ADR-0073 — The age ramp is a calendar, corrected only as far as it must be

**Status** accepted · **Amends ADR-0064.** Implemented in `polis_layout::age`.

ADR-0064 replaced growth-sequence rank with real commit time and was right to.
It also assumed PRD §7.1's premise — that a repository's first year produces
enough of its files to *be* an old town — and that premise is not true of a large
class of real repositories. Measured through one build:

| repository | files | first year | files in the core band, calendar ramp alone |
|---|---|---|---|
| Neovim | 3 890 | 36.3 % | 36.3 % — the calendar ramp is already right |
| Django | 7 014 | **3.4 %** | **3.4 %** |

Django is not a repository without a history. It is a repository whose history
*accelerated*: 235 files survive from its first year and 6 779 from the twenty
after. Drawing 3.4 % of it as the core and the other 96.6 % at one rim grain is a
true statement about the calendar and a useless map — the judge's word for the
result was "terrazzo", and the reason is that a gradient the eye cannot find is
not a gradient. The measured age gradient (rim block area ÷ core) was **1.65×**
against Neovim's 5.96×, from one generator with no per-corpus tuning.

**Two ramps, blended by one weight.**

* **The calendar ramp** is ADR-0064's, unchanged: the first year owns
  `OLD_TOWN_BAND` of the grain range, everything after spreads over the rest by
  real elapsed time.
* **The equalised ramp** is the share of the repository *strictly older* than
  this file, rescaled so the newest ground is 1. Files sharing a commit time
  share a value, so a tree imported wholesale in one commit lands at exactly 0 as
  one cohort — which is precisely the property ADR-0064 rejected rank for not
  having. Tie-collapsing is what makes rank-over-time safe; rank over *list
  position* is still refused, and for the same reason.
* **The weight** is the smallest `w` on a 64-step grid at which the blend gives
  the map both a core and a periphery with real mass: the core band holds at
  least `CORE_SHARE` (25 %) of the files and the rim band at least `RIM_SHARE`.
  `w = 0` — pure calendar, byte-for-byte ADR-0064 — whenever the repository's own
  growth curve already does that.

Equalisation is the standard cartographic answer to a lopsided distribution,
quantile classification, and its cost is stated exactly: the **ordering** is
preserved perfectly (both ramps are non-decreasing in commit time, so their blend
is, and the module test asserts no inversion), and what is sold is the
**spacing** — the claim that equal distance on the ramp is equal elapsed time.
`AgeRamp::equalisation` is how much of that claim was sold, it is reported in
`CityReport` and on the `HISTORY` line of `polis snapshot`, and it is `0`
wherever it did not have to be paid.

**A thermostat, not a servo.** The correction fires at
`CORE_SHARE − CORRECTION_DEAD_BAND` and then aims at `CORE_SHARE`. Without the
dead band a repository whose core band already holds 23.6 % against a 25 % target
buys the last 1.4 % with a *different city* — every plot separation moves, which
is what PRD §7.7 forbids. Measured: the 1 000-file corpus sits exactly there, and
correcting it moved one package onto two faces for seventeen files of core. It
does not soften the case this exists for; Django's 3.4 % is not near the band and
no dead band reaches it.

**A cliff in ADR-0064 that the blend exposed and `TAIL_FLOOR_MS` fixes.** The
calendar ramp's second branch divides by `span − YEAR`. At 366 days of history
that denominator is *one day*, so the single file added on the last day is
pinned to `t = 1` while its neighbour a day earlier sits at 0.40 — a repository
one day past its first birthday drew a periphery out of one file. ADR-0064's
claim of continuity was true of the *span* and not of the ramp. The tail now
never spreads less than six months of history over the upper band, which makes
`absolute` continuous in the span at every point rather than only in the limit,
and binds for exactly the first eighteen months of a repository's life.

**What each degenerate history degrades to** — the part that had to be decided
rather than discovered, restated because the answers have changed:

| History | Kind | Result |
|---|---|---|
| ≥ 1 year, first year a real share of the files | `Calibrated`, `w = 0` | PRD §7.1 literally, and identical to ADR-0064. **Neovim: `w = 0`.** |
| ≥ 1 year, first year a sliver | `Calibrated`, `w > 0` | The oldest quarter of the repository is the old town. **Django: `w = 47 %`, core files 3.4 % → 26.2 %, age gradient 1.65× → 2.33×.** |
| 0 < span < 1 year | `Relative` | Every file is inside its own first year, so the calendar puts them all within `OLD_TOWN_BAND · span/YEAR` of zero — one grain, no periphery. It is the **rim** condition that fails here, and the same mechanism that gives Django a core gives a young repository a fringe. The reading is relative: "oldest in *this* repository", not "older than a year". `MIN_SPREAD`'s special case is gone, subsumed. |
| one commit, or a squash import with nothing since | `Uniform` | There is no age information, so **none is drawn, and no amount of equalisation may invent any**: one grain everywhere, `NO_HISTORY`. This is the case the dead band and the weight search never see, and it is the reason the equalised ramp is over *commit time* and not over list position — falling back to list position would draw `git log`'s within-commit path order as history, and the operator would read alphabetical order as age. |

Three determinism properties survive unchanged and one is added: no clock is
read; the running maximum keeps the ramp monotone across a merge; every value is
quantised to `1/4096` before it leaves the module; and the weight is a
**quantised search over a fixed ascending grid, first candidate wins** — not a
solve, because a closed-form root of a piecewise-linear inequality lands on a
different `f64` on a different target (PRD §7.4).

*Consequence for the corpora.* The synthetic fixture (8.2 % first-year) is
corrected by 45 % and its measured age gradient is 2.96×; the 1 000-file corpus
is corrected by 0 %. The M1 gate's four-history test still produces four
different cities, and it must: the blend is a function of the *distribution*, so
a young repository and an accelerating one get different weights on top of
different calendars.

---

## ADR-0074 — The district partition is a partition of the plot **graph**, not of the plane

**Status.** Accepted. It removes `territory.rs`'s polygon layer entirely and adds
`polis-layout/src/regions.rs`.

M1 failed three gates on the same picture. The renderer was retoned twice, and an
independent reviewer measured the structural edge difference between two of those
rounds at 2.3 % RMSE: the geometry had not moved. What was measured, on five
corpora, was

* silhouette solidity **0.9947–0.9994**, where a convex outline is 1.00;
* four to nine district borders per city running dead straight from inside 5 % of
  the radius out past 90 % of it, within two degrees of radial;
* on every real repository, one stroke passing within 2.3 % of the radius of the
  civic square with its ends on opposite bearings — two avenues fused into a
  boulevard across the middle, which `territory.rs`'s own documentation claimed
  the civic square prevented.

### The cause was structural, and it was written down as a virtue

`territory::build` made the root face a wobbled 19-gon's **convex hull** and
fanned it into up to nine sectors around the civic square, then chord-split each
sector down the directory tree. The module called root convexity "load-bearing",
and it was: a plot could settle only inside its own district's polygon, so the
partition had to tile the city, and a convex region cut by chords has a convex
silhouette and straight borders. Every property the gate failed on is a theorem
about that construction rather than a tuning miss. No parameter reaches it.

### The decision

Contiguity is a property of a **graph**. The plots are already settled, and their
Voronoi cells already have an adjacency relation — the one the finished map draws
as roads. So:

> Grow freely, with two hard rules and no polygon: the packing distance, and a
> connectivity reach from the nearest settled plot. Then partition the **plot
> adjacency graph** recursively down the directory tree, balanced by plot
> capacity.

`regions::partition` is that partition. A split picks two seeds — the oldest
ground in the set, and the vertex furthest from it by shortest path — runs
Dijkstra from each, and cuts on the difference `d_a - d_b`. **Both halves of such
a cut are connected for any threshold**, and the proof is three lines in the
module. So the cut point is free to be chosen purely for balance: connectivity is
not traded against it, it is free. Distances are integers in thousandths of a
world unit, so the sort key is exact and two runs cannot disagree in the last bit
(PRD §7.4).

Nothing in `regions.rs` touches a coordinate. There is no centre and no radius
anywhere in it. The outline of the city is the outline of the ground the growth
settled, and every district border is a chain of Voronoi bisectors between two
settled plots.

`territory.rs` survives as the directory tree and nothing else: `RIM_SIDES`,
`WEDGES`, `fan`, `ray_to_rim`, `AVENUE_SPLAY`, the civic-square fan, the ring
roads, the quarter-clipped Voronoi, `conform_avenues` and `avenue_masks` are all
gone. The Voronoi is bounded by a **phantom lattice** rather than by a city
limit: the boundary between a real cell and a phantom one is the perimeter road,
so the town has an edge without anything having drawn one.

### What it bought, measured on four corpora

| | synthetic 5 000 | Django | Neovim | CPython |
|---|---|---|---|---|
| solidity, was 0.995–0.999 | **0.825** | **0.809** | **0.773** | **0.764** |
| radial spokes, was 4–9 | **0** | **0** | **0** | **0** |
| boulevards through the middle, was 1 | **0** | **0** | **0** | **0** |
| longest dead-straight district border | **11.1 %** | 8.5 % | 8.4 % | 6.7 % |
| 4-and-5+ junction share, was 45–57 % | 66.9 % | 67.9 % | 66.1 % | 65.5 % |
| through-streets past a quarter of the diameter | 21 | 27 | 25 | 10 |

### Why the *third* row is in the table

It is the row that did not exist before this round, and it is the one that
decided which of two competing geometries landed.

The other attempt kept a partition of the plane and replaced the radial fan with
balanced recursive **chord** subdivision — children split at the index that best
balances quantised weight, on a normal perpendicular to the longest axis. It
scores `radial_spokes = 0`, honestly. Its longest dead-straight district border is
**60.0 %** of the city diameter on the synthetic corpus, 59.5 % on Django and
54.4 % on Neovim, against 46.3 / 93.4 / 45.6 % for the radial partition it
replaced. Its Django render is one green half and one purple half divided by a
ruled line running edge to edge.

A chord of a convex face is a straight border whatever bearing it is given, so a
test that only asks whether the ruler pointed at the middle can be answered by
turning the ruler. `Structure::straight_border` and `straight_borders` ask the
question with the bearing taken out of it, and the M1 gate asserts the count at
**zero**. The two populations are an order of magnitude apart — 6.7–11.1 % for
bisector borders against 46–93 % for chord and wedge ones — so
`STRAIGHT_BORDER_SHARE` is set at a fifth of the diameter, in the gap between
them and beside neither.

### The costs, stated

**`fragmented_subtrees` is a rate now, not zero.** Districts and packages — the
two contiguity properties PRD §8 and §9 name — are **0 on every corpus**, and
`regions` guarantees them by construction at the level where packages are
divided. An *intermediate* directory is a union of districts, and below the
package level the cut goes where the balance wants it, so a sibling's parcel can
land between two branches of the same middling directory. Measured 25 of 312 at
5 000 files, against 102 of 276 *districts* in the design bake-off's prototype.
The gate holds `fragmented_subtrees × 8 ≤ districts` and the number is printed.

**`settled_nonadjacent` rose from 1 to 138 of 999 plots.** It is no longer a
contiguity guarantee — `regions` provides that on the graph — but a shaping
number: how much work the partition has to do. It is asserted as a rate,
`× 5 ≤ plots`, rather than deleted. `settled_on_fringe` is asserted at **exactly
zero** and kept, because there is no longer a polygon to be on the fringe of: a
non-zero value would mean one had come back.

**The growth is the expensive stage.** Free frontier growth evaluates roughly
2.7 million candidate positions on Django where the polygon-constrained search
evaluated far fewer. Cold start on Django is 2 559 ms against the previous
1 932 ms and PRD §13.1's 3 000 ms budget, and the whole of the difference is
`accrete::grow`. Half of what it cost when the graft first landed was recovered
by letting the candidate scan stop at the first plot inside the packing distance:
the verdict cannot change afterwards, so the early exit is exactly equivalent,
and the layout digest is unchanged across it.

**Incremental reuse fell.** A growth step that founds a new plot moves its
neighbourhood, and a moved block ring is a different cache key, so building reuse
on a founding step is 34–38 % where the polygon-clipped Voronoi kept 87 %. Steps
that only fill an existing plot still reuse 99–100 %. At the scale the budget is
written against, `tests/incremental_budget.rs` measures 62 % of cuts and 76 % of
buildings with seven of 1 003 nodes moved, median 45.7 ms and p95 49.4 ms against
the 50 ms bar. `a_warm_cache_and_a_cold_one_build_the_same_city` now reads reuse
after **every** step and asserts the mean, because reading only the last one
measured whichever kind step five happened to be — a coin toss, not a property of
the caches.

### Determinism

Unchanged and re-proved. The synthetic 5 000-file layout is byte-identical across
two fresh release processes, a **debug** build, and a run under
`TZ=Pacific/Kiritimati LC_ALL=tr_TR.UTF-8`: digest `e02c2e3daee995f6`, layout
sha256 `ed8a580b…`, PNG sha256 `48b1b487…`. The three new pieces were written to
keep it: `regions` sorts on integer distances; the phantom lattice is anchored at
the origin rather than on the settlement's bounding box, so one added file cannot
move the whole edge of town; and `accrete`'s spatial hash is a hand-written
open-addressing table with a fixed integer mix rather than a `HashMap`, because
`std`'s `RandomState` is seeded per process — which is the hazard the crate's
`BTreeMap` rule exists to prevent. Nothing iterates it: both readers reduce to
minima and counts, and the one that returns a list sorts it.

---

## ADR-0075 — A district that settled ground appears on the ground, and a file the block has no room for takes a parcel next door

**Status.** Accepted. Two defects found by instrumentation once ADR-0074's
partition met the reworked age ramp and lot subdivision. Both fixed at the cause.

### A district with plots but no face

`blocks::assign` gives each face to the district whose plots sit deepest inside
it. The vote is taken one face at a time, so a district every one of whose faces
also holds a deeper-voting neighbour can win **none** of them. `regions` cannot
prevent it: it guarantees a connected set of *plots*, and this is a fact about
*faces*, which the collapse and the prune have moved since.

It was invisible until the age ramp began seating 999 plots on the 5 000-file
corpus where it had seated 955. Two districts of 312 lost their ground, and eight
files of `core/media/user/account` became buildings standing in a stranger's
quarter — which PRD §8 draws as a directory that is simply not on the map.

`blocks::seat_unseated_districts` runs the same rule once more with the winners
fixed: each unseated district takes the one face where its own plots voted
highest, and only from an owner that keeps at least one other face. That second
clause is what makes it terminate and what stops it unseating anyone in turn —
the number of seated districts strictly rises and no district is ever emptied.
Every directory that owns a file now has ground, on all four corpora. (311 of 312
districts are drawn at 5 000 files; the one that is not,
`vendor/store/pool/buffer`, owns no file of its own, and all of its children are
present.)

### Two files on one parcel

Measured at 11 files of 7 014 on Django and 21 of 6 138 on CPython. A file
sharing a parcel has no building, and a file with no building has silently left
the map.

Instrumented rather than swept, and the print is the whole diagnosis:

```text
DIAG block=1540 plots=1 caps=[(5, 43)] files=43 viable=12 area=1.0274 verts=3
```

One plot, whose capacity is five, carrying **43** files, in a triangular face of
area 1.03 against a median block of 4.81. `django/utils` is 43 files, and
`regions` gave it a single plot: the partition divides plot capacity down the
tree, but every district also has a **floor** of one plot, and Django is 2 076
directories over 2 472 plots — so for most leaf directories the floor, not the
balance, is what they get. The face has room for twelve buildings at the local
grain, and twelve is what it produced.

The subdivision and the densify loop were both instrumented before anything was
changed, and both had run to exhaustion: every parcel the ground can hold already
existed. Raising the densify round budget from a constant to one that scales with
the file count was tried first and changed the layout digest not at all, which is
what ruled that cause out — the budget is now demand-scaled anyway, because a
bound on work that ignores the demand is the wrong shape of bound.

So the fix is not there. `lots::rehouse_overflow_next_door` walks outward over the
road graph from the crowded face and gives the surplus file the nearest **vacant**
parcel, preferring one in its own district, then one in its own top-level package,
then any, up to four rings of blocks. The ground exists: Django's map has 3 249
vacant parcels. This is the relaxation ladder PRD §7.2 already applies to plots,
applied one level down to parcels, and it is strictly better than what it
replaces — a parcel of its own on a neighbour's ground beats no building at all.

It does not hide the shortfall. `shared_faces` still counts the districts whose
own ground could not hold their files — 1 on Django and Neovim, 3 on CPython, 0
on the synthetic corpus — and the report prints it. `overflow` is **0** on all
four, and `buildings = files − massed` exactly.

---

## ADR-0076 — A block is cut on **two** perpendicular directions, and a building is a rectangle

**Context.** PRD §7.2 stage 4 says "Lots by recursive subdivision of each block
along its longest axis until lot area falls under target", and stage 5 says
"Buildings are lots inset by a setback". Both were implemented literally and both
were wrong in the same way, and the result failed the visual gate five times
running under five different names — "crazed glaze", "shards in a void",
"cracked mud", "lichen ribbons", "gravel", "confetti", "mould on a dark plate".
An independent reviewer finally named it, and a 5× brightened crop of the Django
render confirmed it: **blocks resolve into asterisks.** Five to eight
wedge-shaped buildings radiating from a seed point inside each block, at every
zoom, in every render. Almost no building anywhere on the map was a rectangle.

Two mechanisms, one artefact.

*Stage 4.* `longest axis` was read as **this ring's** longest axis, recomputed at
every level of the recursion from `geom::longest_axis` — the principal axis of
the ring's vertices. A block from `accrete` is a Voronoi-ish cell: six or seven
sides, no dominant direction, and a principal axis that is nearly degenerate. Cut
it once and each half is a different polygon whose own principal axis has turned
twenty or thirty degrees; cut those and it turns again. After three levels the
cuts fan out from the middle of the block. The generator was not a Voronoi of
interior seeds, but the recursion reproduced one.

*Stage 5.* The footprint was the **band** of the lot's inset interior within
`depth` of its street edge — the lot's own shape, cut off at the back. Faithful
to the sentence, and it means a wedge-shaped lot inset by a setback is a smaller
wedge. Every building inherited its lot's shape, so the fan was drawn as
architecture.

Measured on Django, 7 011 buildings: **46.0 %** of footprints were
quadrilaterals, at a median area of **0.854** of their own minimum-area bounding
rectangle. A rectangle scores 1.00; a wedge scores about 0.5.

**Decision.**

* **`lots::block_frame`** computes, once per block, the two perpendicular axes of
  the block's **minimum-area bounding rectangle**, and every cut at every depth
  of `subdivide` and `subdivide_weighted` runs along one of them. Which of the
  two is still chosen by which way the piece is longer — that *is* PRD §7.2's
  "along its longest axis", measured in a frame the block sets rather than one
  that rotates under the recursion. The minimum-area rectangle and not the
  principal axis, because its long side is always collinear with a hull edge
  (Freeman–Shapira) and a hull edge of a block **is a road**.
* **`buildings::fit_footprint`** returns a rectangle by construction: the largest
  street-fronting rectangle the lot's per-edge inset interior will hold, its
  depth fitted on a fixed grid and then bisected to the wanted area, its width
  trimmed symmetrically only when even the shallowest allowed rectangle is
  bigger than the file needs. `rect_span` is exact rather than sampled: the
  region is convex, so its left boundary is convex in the depth and its right
  boundary concave, and the extreme of each over a slab is reached at one of the
  slab's two ends.
* **`buildings::frontage_faces`** builds in the **parcel's own** frame — the axes
  of its minimum-area rectangle, which for a parcel this pipeline cut are the
  block's two axes — and lets the street choose only which of that frame's four
  sides the building stands on. Fitting in the *road's* frame instead, inside a
  parcel cut in the *block's* frame, wedges the rectangle between two skew sides
  and cost median lot fill 67 % → 47 %.
* **The ±4° is applied to the finished rectangle**, about its own centre, and
  `shrink_into` scales it about that same centre until it is back inside the lot.
  A uniform scale about a point maps a rectangle to a rectangle, so neither the
  turn nor the containment guard can cost the footprint its shape. **The angle is
  scaled by `looseness`** — zero on terraced ground, the full ±4° on detached
  ground — because a rectangle turned 4° inside a terrace strip pulls its sides
  in by `depth · tan 4°`, which is an order of magnitude more than `PARTY_WALL`,
  and a terrace that has lost its party walls is gravel. A terrace cannot rotate;
  that is what a terrace is. `Building::rotation` publishes the angle that was
  used, never the one that was drawn.

**Consequences.** Measured on Django after the change: **99.96 %** of footprints
are quadrilaterals at a median **0.999** of their own minimum-area bounding
rectangle, p10 0.997. The pinwheel is gone from a 5× brightened crop, and the
dense quarters read as rows of rectangles along their streets.

It is paid for in floor area, and the price is real and was measured rather than
guessed. Blocks hold about 4.5 lots each, so nearly every lot touches the
irregular block boundary and is a rectangle clipped by it — a trapezoid, at a
median 0.894 of its own bounding rectangle. A rectangle cannot fill a trapezoid,
so Django's building coverage falls **32.2 % → 27.3 %** (core 39.7 → 34.3, rim
25.4 → 21.9). That is still above the gate's 25 % floor for a repository of that
size, the core/rim gradient is unchanged at about 1.6×, and `on-road`,
`outside-lot`, `unbuilt` and `overflow` all remain 0. Two thirds of that loss was
bought back before accepting it: `MAX_DEPTH_SHARE` 0.90 → 0.97 (with rectangles
the courtyard comes from the taper the building cannot occupy, not from a back
garden strip), and `FACE_PREFERENCE`, which lets the cross-street orientation win
where the plot's shape makes the road-facing one a sliver — but only when it is a
fifth better, because a row of buildings that all face the same way is the whole
reason a street reads as a street.

Two floors exist because the shape can defeat a naive fit and a file with no
building is the worst outcome this pipeline has. `kerb_line` steps the building
line back over a fixed ladder on a plot that comes to a point at the street, and
`DEPTH_FLOOR_SHARE` lets the depth grid reach below `MIN_DEPTH_RATIO`'s
razor-strip floor — on a skewed plot, measured on a 6 × 0.9 strip whose long
sides are not parallel, the kerb slab and the back slab do not overlap at all and
a grid anchored at that floor has no rectangle anywhere on it.

The depth is **bisected inside the grid bracket** rather than taken from the grid
itself. Landing on the grid overshoots the wanted area by up to one step, the
overshoot is paid sideways, and sideways is a party wall: measured on the terrace
fixture it opened a 0.022 gap where the contract is 0.012.

This invalidates every golden layout in the repository, on purpose, and they were
regenerated.

---

## ADR-0077 — What a building's height means on a clean tree (PRD §17, open question 4)

**Context.** PRD §7.3: "**Height** ∝ uncommitted diff lines. The city rises as
agents work and settles when you merge. The tallest thing on the map is the
biggest unreviewed pile." PRD §17 open question 4 asks whether that destroys the
"recently active" reading. It is worse than that: **a freshly-cloned repository
has no uncommitted diff at all**, and that is the common case — the map is opened
before any agent has run. Read literally, the primary encoded quantity is then
constant across the whole city and carries nothing.

The two-register answer (settled massing from file size, work from uncommitted
lines) was already in place and was not enough. The visual review measured the
legend of three of four renders reading `TALLEST … H = 6.0`, no visible skyline,
and every extrusion side-wall at the same depth. The cause was in the ramp: the
settled register ran linearly in the fourth root from `(1/16)^¼` to `16^¼`, which
puts a file of *exactly* the repository's median at 0.333 of the register rather
than 0.5. File sizes are roughly log-normal, so the *typical* building sat in the
bottom third of the only channel a clean repository has, while
`polis_render::plan::height_ramp` spends 80 % of its tone on that register.
Django, clean: min/median/max **1.00 / 1.92 / 4.00**, monuments at 6.00.

**Decision.** Height on a clean tree is the **settled register**, and it is a
documented combination of exactly three things:

1. **File size against this repository's own median**, on a ramp that is
   symmetric *about that median* — two straight segments in the fourth root
   meeting at `ratio = 1` — so half the city stands above the middle of the
   register and half below. Fourth roots and division only: no `powf`, no
   logarithm (PRD §7.4, determinism rule 4).
2. **A path-seeded storey of variety** (`STOREY_JITTER`), so a street of
   same-sized files is a skyline and not a wall.
3. **The decayed ghost of recent churn** (`BuildingSpec::ghost_lines`,
   `GHOST_HALF_LIFE_HOURS`), which rides the *work* curve rather than a second
   ramp — so a file merged an hour ago is still visibly taller than one untouched
   for a year and slides down continuously instead of snapping. A repository
   nobody has touched since the clone has none, correctly: nothing recent has
   happened in it.

The register was widened, `SETTLED_CEILING` 4.0 → 6.0, and `MONUMENT_HEIGHT`
6.0 → 7.0 to stay above it. The ordering that makes PRD §7.3's sentence literally
true is preserved and is now a chain with margin at every link:

```text
settled 1.0 … 6.0  <  monument 7.0  <  one uncommitted line 8.55  …  64.0
```

`work_height(1) = 7.55` already exceeds `MONUMENT_HEIGHT`, so **any** building
with a single uncommitted line outranks **every** monument and every settled
building, and among them the biggest pile is the tallest thing on the map.

**Consequences.** Measured on Django (7 014 files). Clean tree:
min/median/max **1.00 / 3.23 / 7.00** over 3 482 distinct heights, deciles
1.00 · 1.07 · 1.62 · 2.25 · 2.77 · 3.23 · 3.70 · 4.14 · 4.66 · 5.39 — a skyline
rather than a wall, with the median roof now at half the tone the renderer
reserves for it instead of a quarter. The same tree with three files carrying
4 041 / 1 200 / 12 uncommitted lines puts those three at **64.0 / 50.4 / 16.6**,
each clear of the 7.00 monument ceiling, and the tallest building on the map is
the biggest pile at 9.1× the tallest monument.

On a clean tree the tallest thing on the map is therefore an **orientation
anchor** (PRD §8), not a pile — which is the truthful answer when there is no
pile, and it is why the monument floor sits between the two registers rather than
inside either. PRD §7.3's sentence holds whenever there is a pile at all, and is
never asserted when there is not.

`polis_render::plan::height_ramp` reads `SETTLED_CEILING` at runtime rather than
hard-coding it, so widening the register moved no tone. What it did move is the
cast shadow and the massing cut, both of which scale with height in world units —
which is the channel the review found empty.

Amend PRD §17 to record open question 4 as **closed**.

---

## ADR-0078 — A quarter's founding floor scales with the town, or a small repository has no coastline

**Context.** `m1_gate` asserted the M1 acceptance table against
`polis_repo::synthetic` and nothing else; the real-repository test checked four
trivial properties. Run the same table against this workspace's own hundred-file
checkout and three criteria fail:

| corpus | files | solidity | longest stroke | straight borders |
|---|---:|---:|---:|---:|
| `synthetic::repository(200)` | 200 | 0.763 | — | 0 |
| **this workspace** | 112 | **0.918** | **77.6 %** | **1** |

Solidity 0.918 is the coin PRD §7.2 and three M1 gates exist to prevent, on the
first city a new user ever sees — their own repository.

It is **not** a size effect, and that is the finding. `click`, a real repository
of 166 files, measures 0.725 with the same code. Two real repositories of nearly
the same size, one convex and one not:

| repository | files | top-level packages ≥ 12 files | quarters founded | solidity |
|---|---:|---:|---:|---:|
| `click` | 166 | `tests` 47, `docs` 41, `examples` 39, `src` 18 | 4 | 0.725 |
| this workspace | 112 | `docs` 30, `polis-layout` 20 | **2** | **0.918** |

`accrete::QUARTER_FILES` — "smallest absolute size, in files, for a package to
found a quarter" — was an absolute **12**. Twelve files is a quarter of a percent
of Django and **eleven percent** of a hundred-file repository. This workspace has
ten top-level packages and eight of them are 2–10 files, so eight of ten seed off
the civic square, one settlement grows isotropically, and the outline of a
compact blob is a blob.

The renders say it plainly: `click` has a south-western peninsula, a western lobe
joined by an isthmus and two deep bays; this workspace was a rounded sixteen-gon.

**Decision.** The floor scales with the town, because what it is really bounding
is the **causeway**: a quarter has to be worth the road settled out to it, and a
causeway's length is a multiple of the separation, which scales with the town.

```rust
let causeway = QUARTER_FILES                                   // 12
    .min(total_files.div_ceil(QUARTER_MIN_DENOM))              // 1/20 of the town
    .max(QUARTER_FILES_FLOOR);                                 // never under 3
let floor = causeway.max((total * QUARTER_SHARE).round());     // 1.2 % of the town
```

`QUARTER_MIN_DENOM = 20` says a package worth a road is at least a twentieth of
the repository. `QUARTER_FILES_FLOOR = 3` stops a twenty-file repository founding
a quarter per file, which is the same failure approached from the other side.

**Consequences.** The scaled term **only binds below 240 files** (`240/20 = 12`),
so every corpus at or above that count is byte-identical. That is checked rather
than claimed — `pytest` (690), Neovim (3 890) and Django (7 014) produce the same
digest either way. Measured:

| repository | files | quarters before → after | solidity before → after | longest stroke |
|---|---:|---:|---:|---:|
| this workspace | 112 | 2 → 9 | **0.918 → 0.784** | 77.6 % → 47.9 % |
| `click` | 166 | 4 → 5 | 0.725 → **0.680** | 38.0 % → 41.9 % |
| `pytest` | 690 | — | 0.831 (unchanged) | 62.1 % |
| Neovim | 3 890 | — | 0.756 (unchanged) | — |
| Django | 7 014 | — | 0.773 (unchanged) | — |

The `hamlet` and `town` golden files change and were regenerated with this
change; `hamlet` (3 files) is unaffected because no package clears three.

The wider lesson is ADR-0080's: this defect was reachable only on a *real* small
repository, and the corpus that could not show it is the one the gate was
asserted against.

---

## ADR-0079 — The block-size hierarchy bar stays a floor at 8×, and the 30× target is refused with measurements

**Context.** `JUDGEMENT.md` sets a 30× target for block-size hierarchy
(`p95 / p05` block area). `m1_gate` has asserted **8×** for three rounds without
either meeting the target or arguing against it, which is how a number gets
inherited.

**Decision.** The bar stays a floor at 8× on the generated corpus, and it is a
floor rather than a target because the hierarchy is a property of the
**repository's age spread**, not of the generator. Measured with one build:

| corpus | files | first-year files | p95:p05 | age gradient |
|---|---:|---:|---:|---:|
| `click` | 166 | 25.9 % | **56.0×** | 8.48× |
| this workspace | 112 | 95.5 % | 19.1× | 2.48× |
| Neovim | 3 890 | 36.3 % | 13.0× | 5.96× |
| `synthetic::repository(5 000)` | 5 000 | 8.2 % | 9.4× | 2.35× |
| Django | 7 014 | 3.4 % | 9.1× | 1.65× |
| `pytest` | 690 | **0.1 %** | **7.0×** | 1.31× |

`click` has a real old town — a quarter of its files are from its first year —
and its blocks span 56×. `pytest` rewrote itself: one file of 690 survives from
its first year, so there is no old grain for a coarse rim to contrast with, and
it spans 7.0×.

A 30× bar would fail **three of the five real repositories** measured, for having
the wrong history. The layout cannot fix that, and the one way to manufacture it
is the thing `polis_layout::age`'s `Uniform` case exists to forbid: ramping on
list position, which draws `git log`'s within-commit path order as history and
invites the operator to read alphabetical order as age.

**Consequences.** The number the gate holds is "a gradient still exists",
asserted below the generated fixture's own 9.4×, and the *response* of the ramp
to history is asserted separately and much more strongly by
`the_age_ramp_follows_real_commit_time`. The measured spread is now printed for
every corpus (`POLIS_SHAPE`), so the next round argues from the table rather than
from the target. `JUDGEMENT.md`'s 30× should be amended to "8× floor, spread
reported": it is a real number about `click` and not a requirement any repository
can be held to.

---

## ADR-0080 — The gate runs on real repositories, recorded as manifests

**Context.** PRD §16 asks for "fixture repos with pinned git history". The suite
had two, both written by `tests/make-fixtures.sh`, plus a synthetic corpus
generator. **Every structural number in the M1 acceptance table** — solidity, the
ruler test, coverage, the longest stroke, the junction share — was asserted
against the generated corpus, and `the_real_repository_holds_the_gate` checked
four trivial properties: that two runs agree, that a block exists, that a cycle
exists and that there are twenty buildings.

That is the shape of a gate a generator can flatter, and ADR-0078 is the proof
that it did.

**Decision.** Three real repositories are checked into `tests/corpora/` as
**manifests** and the full acceptance table runs on all of them, in CI, with no
network and no clone:

| corpus | files | commits | span | what only it covers |
|---|---:|---:|---:|---|
| `click` | 166 | 3 333 | 12 y | the small repository, with a real old town |
| `pytest` | 690 | 17 715 | 18 y | a history that accelerated: 0.1 % first-year files |
| `polis-day-one` | 107 | 9 | **1 day** | a repository younger than a day |

A manifest (`polis_repo::manifest`) is one line per file: logical path, size,
class, growth index, added-at, last-touched. **That is the whole of the layout's
input** — no file content reaches it — so the record is not an approximation of a
real repository for layout purposes, it is the thing itself. A vendored checkout
would add megabytes the layout never reads, another project's licence, and a
`.git` directory that would still have to be vendored for the growth order.

`polis_repo::manifest::capture` writes what `load` reads, from the product's own
walk, classifier and growth-order reader — not a second implementation of them —
and `the_corpus_manifests_round_trip_byte_for_byte` asserts the file survives a
round trip.

**Consequences.** What a manifest deliberately does not carry is file content, so
a corpus fixture exercises the city and **not** PRD §9's streets; `hamlet` and
`town` stay, and they are what covers `tree-sitter`.

The live checkout is still laid out, and it is asserted at `Bars::Invariants` —
topology, placement, contiguity, determinism — with the shape statistics
reported, not asserted. The reason is measured: at a hundred-odd files those
statistics have several points of sampling noise, because the convex hull and the
junction census are decided by a handful of blocks. The same working tree, two
files apart:

| files | 4-and-5+ share | solidity | straight borders | longest stroke |
|---:|---:|---:|---:|---:|
| 112 | 47.2 % | 0.784 | 0 | 47.9 % |
| 114 | 44.6 % | 0.833 | 1 | 64.9 % |

This checkout gains and loses files on every commit, so a hard geometric bar on
it fires on whoever happens to add the unlucky file. `polis-day-one.corpus` is a
pinned capture of this same repository and it holds that ground at full bars.

Two smaller consequences worth recording:

* **`actions/checkout` needs `fetch-depth: 0`.** It is shallow by default, and
  PRD §7.1 makes `git log` the growth order — so in CI the live-checkout leg was
  about to lay out a repository whose every file was added by one commit, which
  is the easiest case there is, and report a pass. All three jobs now fetch the
  full history and `ci_runs_the_golden_test_on_ubuntu_and_windows` asserts it.
* **Coverage has a scale.** The 25 % bar was only ever asserted at 5 000 files.
  Below a thousand the same code measures lower and always has —
  `synthetic::repository(100)` was at 21.5 % before anything in this round moved
   — because a small repository parcels more ground than it has files to fill and
  the remainder is PRD §7.5's vacant lots. The bound below a thousand files is
  stated at 18 % rather than pretended to be 25 %.

---

## ADR-0081 — An empty face belongs to the district around it, so the metric and the renderer see one map

**Context.** `blocks::assign` gives a face with no plot inside it
`district: None`, and `blocks::publish` turned that into `LogicalPath::root`. So
the renderer painted it in the **root district's** colour, drew a district border
all the way round it, and counted it as root's ground when deciding which edges
are borders — while `districts::fragmented`, which reads `BlockPlan::district`,
skipped it entirely.

Django ships ten such blocks, Neovim one, `click` three. Ten blocks of root
scattered across a map is exactly the fragmentation the metric exists to report,
and the metric could not see them. **A metric that measures something other than
what is drawn is worse than no metric**, because it is evidence.

**Decision.** The fallback is removed rather than the metric taught about it: an
empty face inside a district is that district's ground, which is also what it
looks like. `blocks::settle_open_blocks` runs at the end of `heal_districts` —
the last word on district shape, taken on the object the metric reads — and gives
every remaining district-less block the district beside it, by **multi-source
breadth-first search from every seated block at once**.

**Consequences.** A block is only ever given the district of a neighbour that
already has it, so the district it joins gains a block adjacent to one of its own
and stays one piece; the same argument covers every ancestor, so
`fragmented_subtrees` and `fragmented_packages` are preserved too. Ties are
broken by the seeded queue's order — block id ascending, neighbours ascending —
so the answer is a function of the block list alone (PRD §7.4).

After this, `publish`'s root fallback is unreachable on any city with a file in
it, and the gate says so directly rather than by inspection: the published
blocks summed over districts equal the block count, asserted on four corpora.

---

## ADR-0082 — The cold-start budget has two drivers, and a genuine 5 000-file repository is inside it

**Context.** PRD §13.1 budgets "cold start → first frame, 5 000-file repo: < 3 s".
Django measures at the line, and the previous round waved that away with "Django
is 7 014 files". That is true and it is not an argument until the interpolation
is shown.

**Decision.** Measure the curve, name the two drivers, and state the answer with
the arithmetic. Five real repositories, release build, this machine, `polis
snapshot`; **cold** means the history and import caches deleted first. Two cold
runs each, and they agree to within 1 %:

| repository | files | commits | walk | history | imports | diff | layout | render | **cold** | warm |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| this workspace | 115 | 9 | 1 | 45 | 180 | 180 | 19 | 304 | **728** | 519 |
| `click` | 166 | 3 333 | 1 | 76 | 162 | 62 | 25 | 289 | **614** | 384 |
| `pytest` | 690 | 17 715 | 3 | 262 | 192 | 108 | 50 | 334 | **947** | 514 |
| Neovim | 3 890 | 37 934 | 9 | 1 015 | 15 | 50 | 207 | 380 | **1 676** | 662 |
| Django | 7 014 | 34 898 | 76 | 987 | 423 | 139 | 931 | 443 | **2 998** | 1 662 |

Cold start has **two** independent drivers and PRD §13.1 names only one:

* **`history` is driven by the commit count**, not the file count: 0.027 ms and
  0.028 ms per commit on Neovim and Django, which have 37 934 and 34 898 commits
  against 3 890 and 7 014 files.
* **`layout`, `imports` and `walk` are driven by the file count.**

Interpolating to a genuine 5 000-file repository, two ways that have to agree:

*Bracketing on files.* Neovim (3 890) and Django (7 014) bracket 5 000, and
Neovim has **more** commits than Django, so the history term is not being
smuggled downward by the choice of bracket:

```
1 676 + (2 998 − 1 676) × (5 000 − 3 890) / (7 014 − 3 890)  =  2 146 ms
```

*Term by term.* walk 33 + imports 302 (Django's per-file rate, a parsed language;
Neovim's C costs 15 ms in total) + diff 90 + layout 464 + render 402, plus
0.028 ms per commit:

```
1 291 ms + 0.028 × commits   →   2 268 ms at Django's 34 898 commits
                             →   inside 3 000 ms up to ~61 000 commits
```

**A genuine 5 000-file repository is inside the budget with about a quarter of it
to spare**, by both routes, and the bound on the second driver is ~61 000
commits — more than any of the five repositories measured has. Django, at 7 014
files (40 % over the budgeted size) and 34 898 commits, measures 2 989–3 007 ms:
*at* a budget it is not the subject of.

**Consequences.** Two things were done rather than only argued.

`History::fold_range` now folds the `git log --name-status` stream through a slot
table instead of straight into a `BTreeMap`. Django's walk emits 155 432 file
lines for some 12 000 distinct paths — thirteen to one — and every line was
paying a `LogicalPath` decode (an allocation and a case fold) and an insert into
a `BTreeMap` of twelve thousand string keys. Each raw spelling now gets a slot on
first sight and every later line is an `ahash` lookup on the raw bytes and one
`Vec` write. Measured:

| repository | history, before | after |
|---|---:|---:|
| Django | 1 712 ms | **987 ms** |
| Neovim | 1 854 ms | **1 015 ms** |
| `pytest` | 809 ms | **262 ms** |

`git log --name-status` on its own, timed at the shell on Django, is 1 131 ms, so
the walk now costs about what git costs and the remaining term is not ours.
Django's cold start moved from 3 787 ms to 2 998 ms on this measurement.

Determinism is unaffected: the map is a lookup table that is never iterated for
output, the slots are drained in walk order recovered from a sequence number, and
two raw spellings that fold to one `LogicalPath` (a case-only rename, ADR-0028)
merge exactly as before and in the same order.

Two caveats, stated rather than buried. The `render` column is PNG encoding in
`polis snapshot`; a real first frame does not pay it, so every figure above is
pessimistic by about 400 ms. And **cold is the once-per-`HEAD` case**: the second
launch on the same commit is the `warm` column, 1 662 ms on Django.

---

## ADR-0083 — `regions::partition` is a tenth of the incremental step, not its remaining lever

**Context.** PRD §13.1 budgets an incremental layout step at under 50 ms.
`tests/incremental_budget.rs` named `regions::partition` as "the remaining
lever", and the sentence was about **churn** — how many buildings move under the
operator (PRD §7.7) — but it was read as a performance claim and carried forward
as one for two rounds.

It is not one. Profiled directly at 5 000 files, release, one stage at a time:

| stage | ms |
|---|---:|
| `Graph::from_cells_indexed` (weld) | 2.13 |
| `regions::adjacency` | 0.15 |
| **`regions::partition`** | **3.41** |
| `Settlement::reseated` | 0.33 |
| `districts::links_to_keep` | 0.28 |
| `collapse_short` + `compact_nodes` | 2.68 |
| `Graph::faces`, first pass | 0.42 |
| `face_districts` + `border_edges` + `choose_prunes` | 3.23 |
| `delete_edges` + `compact_nodes` + `faces` | 0.68 |
| `classify_by_betweenness` | 1.04 |
| `blocks::assign` | 2.58 |
| `blocks::heal_districts` | 0.52 |
| **graph pipeline** | **17.4** |
| `lots::parcel_city` | 30.7 cold / ~9 warm (78 % cache hits) |
| building seating | ~30 cold / ~2 warm (96 % cache hits) |

**Decision.** Record the profile in the test file that made the claim, and say
where the real lever is: the ~17 ms graph pipeline is recomputed in full on every
step, including the six adds of twelve that settle no new plot at all and for
which the cell diagram — and therefore every one of those stages — is unchanged.
The fix is a cache keyed on the cell diagram, not an incremental partition, which
would buy at most 3.4 ms.

**Consequences.** Not taken this round, deliberately: the step is inside its
budget and the change spans `city::assemble`, which this round's parallel work
owns. Measured after this round's changes, release, twelve single-file adds at
5 000 files, on an idle machine:

| rayon threads | median | p95 |
|---|---:|---:|
| 1 | 35.0 ms | **42.4 ms** |
| 2 | 35.4 ms | 38.5 ms |
| 4 | 35.5 ms | 40.4 ms |
| 24 | 35.6 ms | 38.0 ms |

Every row is inside 50 ms, **including on a single core**. The gate's 57.6 ms was
taken at one thread *under load*, and that remains true and remains unfixable by
this file: a budget assertion cannot be made immune to a machine that is already
busy, only given the machine, which is what the test being its own target does.
What has changed is that the next reader has the profile and will not spend a
round making the partition incremental for 3.4 ms.

---

## ADR-0084 — A stale golden and a nondeterministic one are told apart before anything is printed

**Context.** PRD §16 calls the golden-layout snapshots "the most important test
in the suite". Last round all three were **red on arrival**: the layout had
changed and nobody regenerated them, so the first thing anyone saw was a snapshot
diff with no explanation. The reflex is `cargo insta accept`, and that reflex is
right exactly half the time — a layout that differs **between two runs** also
differs from the file, and the diff looks identical.

**Decision.** Each golden test generates its fixture city **twice** and classifies
the result before `insta` sees the value (`m1_gate::classify_golden`):

* Two runs disagree → `Nondeterministic`. Panics with the first differing line
  and a list of the usual causes. It never reaches `insta`, so there is **no
  `.snap.new` to accept** and no way to make it go away by regenerating.
* Two runs agree and the file differs → `Changed`. Prints a banner saying how
  many lines moved, telling the reader to look at a render first, and then lets
  `insta` show the diff.

`classify_golden` is a pure function of `(first, second, stored)` precisely so
the alarm itself is asserted — `a_stale_golden_and_a_nondeterministic_one_are_told_apart`
covers the case that matters most: two runs disagree *while the stored file
matches the first one*, which a naive comparison calls a pass.

**Consequences.** One extra generation of a fixture city, under two milliseconds,
against the cost of accepting a determinism bug by reflex. The trailing-newline
case is treated as a match, so a hand-edited snapshot does not report a layout
change.

---

## ADR-0085 — Hue encodes what kind of code it is, and PRD §8's industrial zone is one of the kinds

**Context.** The renderer keys district hue on the top-level directory
(`plan.rs`, "hue families keyed on the top-level directory"). That tells
districts apart and says nothing else: green and purple are identities, not
meanings. Looking at a rendered city of his own repository the operator said the
colours "carry no meaning", and asked for "the kind of what the neighborhoods
are". PRD §10.3's contrast budget is unchanged and unchallenged — the whole base
map stays inside channel 48 — so hue is the only free channel there is, and
spending it on identity is spending the one thing that was available.

`FileClass` (PRD §8) cannot answer this. It is a *landmark* classification —
monument, industrial, civic square — and it is deliberately sparse: almost every
file is `Ordinary`, so almost every district would be one colour.

**Decision.** A second, orthogonal axis: `polis_repo::kinds::CodeKind`, nine
variants — `source`, `test`, `docs`, `config`, `build`, `assets`, `data`,
`vendored`, `unknown` — decided from the path by six ordered passes, with one
content heuristic (a `@generated` / `DO NOT EDIT` banner in the first 512 bytes).
A neighborhood's kind is the dominant kind of its files, and the full `KindMix`
travels with it, because "60 % test, 40 % source" is a real shape that one label
throws away.

Three things about this are load-bearing:

* **`Vendored` *is* PRD §8's industrial zone**, decided by the existing
  `tree::IndustrialRules`, not by a second list. Generalising the rule rather
  than duplicating it is what keeps `FileClass::Industrial` and
  `CodeKind::Vendored` from ever disagreeing, and it is checked first for the
  reason `classify_with` checks it first: `node_modules` holds ten thousand files
  called `index.js`.
* **The pass order is the design.** A code extension is checked *before* the
  directory hints, so `docs/conf.py` stays source; a directory hint is checked
  before the remaining extensions, so `data/cities.json` is data and
  `src/settings.json` is config; CI directories are checked before both, so
  `.github/workflows/ci.yml` is build and not config.
* **`Unknown` is reported, not folded into `Source`.** "1 819 files match no
  rule" is a fact about the rules, and it is how Neovim's 2 044 `.vim` files were
  found and fixed. A silent default would have hidden it.

**Configurable, because a shipped list is wrong somewhere.**
`.polis/neighborhoods.json` adds to or removes from every table, including the
industrial one. Two known false positives are left in on purpose, with the config
file as the answer: Django ships a `django/test/` package that is production
source and is classified `test`, and a `.po` catalogue is `data` although a
translator would call it source.

**Consequences.** Kind is derived, never stored: `kind_of` is a pure function of
the path, so `FileMeta` gains no field, a serialized `RepoTree` gains no bytes,
and there is nothing to migrate when a rule changes. Measured over seven real
repositories (`docs/neighborhoods-sample.md`), unclassified files are 0–2 % of
each except Neovim before the `.vim` fix.

---

## ADR-0086 — District depth is adaptive, and vendored trees are held out of the sizing budget

**Context.** A district is a directory (PRD §3), and `DistrictTree` takes that
literally — every directory holding a file. The label-and-colour layer picked the
top level instead, so a repository whose code lives under `src/` got one enormous
district and a scatter of tiny ones. Neither end is usable: 13 121 districts is
not a legend, and 70 is not a map of `qurio-toolset` either, because 61 458 of
its files are one `node_modules`.

**Decision.** Choose the depth per branch. Descend into a directory while it
holds more than `max_share` (8 %) of the repository; stop when a child would hold
fewer than a floor derived from the repository's own size
(`max(6, 0.4 % of the files)`). Neither bound is a district *count*: a count
would shred a small repository and fuse a large one, and the observed range is
3 → 68 across the seven repositories tested.

Four rules make it work on real trees rather than on generated ones:

1. **Vendored trees are one neighborhood each and are excluded from the budget.**
   The share and the floor are computed over *civic* files — the total with every
   industrial subtree removed. Without this, `qurio-toolset`'s 88 880 vendored
   files set the ceiling at 7 236 and the operator's own 1 575 collapse into a
   single district, which is exactly backwards. It is also PRD §8 restated: the
   mass is drawn as one mass.
2. **One promotable child is enough to split.** The shape this exists for is
   `src/` beside a lone `README.md`; a rule that required two children would find
   one child and a one-file residual and hand back the blob it was asked to break.
3. **Single-child chains collapse.** `a/b/c` with nothing else in `a` or `b` is
   one place with three names.
4. **Contrast is the second reason to descend.** A directory under the ceiling is
   still split when a child large enough to label would be a *different kind* —
   compared against the district **without** that child, not against the district
   as it stands. Comparing against the whole is the trap: a child big enough to
   decide its parent's kind always agrees with it. Measured on Django, this is
   the difference between ten `contrib` apps reading as `data` — 2 456 `.po`
   files outvoting the Python — and reading as the source packages they are, with
   their `locale` trees beside them.

A name is the shortest suffix no other neighborhood shares, with a floor of two
components where there are two: `components/custom`, not `custom`. The extra
component costs eight characters and is the difference between a label and a
guess. Past 30 characters it falls back to one, so a 40-character storage-bucket
name does not become the label.

**PRD §9 is untouched.** "The tree determines placement" is about placement;
this changes *granularity*. Every neighborhood is still a directory, still
addressed by its path. `RepoIndex::districts` is unchanged and still returns
every directory, because that is the granularity `polis-layout` places plots at.

**Consequences.** Additive: no existing type changed shape, `polis-layout`
consumes nothing new, and the golden snapshots were verified green rather than
regenerated — they were not stale, because the layout did not move. The partition
is a pure function of `(paths, sizes, options)` with every threshold fixed before
the walk, so input permutation cannot move a neighborhood; asserted in unit tests
and checked on all seven repositories across two processes and across debug and
release.

---

## ADR-0087 — A neighborhood description is a quotation from the repository, or it is nothing

**Context.** The operator asked for "the title and the description of what the
neighborhood is doing", and chose **derived-from-repo over an LLM**. PRD §2
independently forbids the alternative: "single operator, local machine, local
data. No server, no auth, no telemetry leaving the box."

**Decision.** Four sources, first one that survives sanitising: a README in the
directory, a package manifest's `description`, a module doc comment on the
directory's anchor file (Rust `//!`, a Python module docstring, a leading JS/TS
block comment — read with the same tree-sitter grammars `imports` loads, through
the same `ts_language`, not with regexes), and finally a synthesised **inventory**
of what is actually there.

The inventory is fenced off by `DescriptionSource::is_prose` and is deliberately
a *statement of fact* — "most imported: `BookContext.tsx`", "67 files named
`cover`" — never a guess at intent, and it is emitted only when it carries
something neither the name nor the colour already shows. When there is nothing
true to say, the answer is `None`: **a wrong description is worse than none**, and
"utils" described as "Utility functions" is the failure mode this rule exists to
refuse. 13–38 neighborhoods per repository get nothing, and that is the design
working.

**Sanitising is load-bearing, not hygiene**, because a description is drawn onto
an image the operator may share. Markup and every C0/C1 control are stripped, and
so are the Unicode bidi overrides and zero-width characters — text that renders as
something other than its bytes is a spoofing channel on a shared picture. A
candidate is **rejected whole** — and the next source tried — if it reads as code
or a bare file path, or if it carries anything credential-shaped: a published key
prefix, an assignment to a name like `api_key`, a URL with a password in it, or a
20-character high-entropy token. Rejection rather than redaction, because a
redacted secret still says *there is a secret in this file*. The plain word
"secret" in a sentence is allowed through; only an assignment to it is not.

**Consequences.** Cached like `imports::ParseCache` — a 128-bit content digest
plus the byte length, written out rather than taken from `DefaultHasher`, so two
machines cannot disagree about a hit. Only the first 16 KiB of a handful of files
per neighborhood is read, and never inside an industrial tree: somebody else's
library does not get to describe a district of this city.

The honest number is the **prose** fraction, not the described fraction. It is
79 % on this repository, where every crate has a `//!` and a `Cargo.toml`
description, and 15–33 % on the operator's own repositories and on Django and
Neovim. That is a fact about how repositories are written, not a gap in the
extractor, and `docs/neighborhoods-sample.md` reports it per repository rather
than averaging it away.

---

## ADR-0088 — A description is rejected when a *tool* wrote it, and a file speaks only for the directory it is in

**Context.** ADR-0087's extractor was reviewed by running it over the operator's
eight repositories rather than over fixtures, and four of its outputs were
indefensible on sight:

* `qurio-toolset/src/components/landing` was described, on the map, as
  **"eslint-disable"**. A leading `/* eslint-disable */` is a block comment in
  exactly the position a module doc comment occupies, and tree-sitter was right
  to hand it over. It is simply not prose.
* Four of eight repositories had their **root** district — PRD §8's civic
  square, the most prominent label on the map — described by a project
  generator: "This is a Next.js project bootstrapped with create-next-app" on
  three, and "This template provides a minimal setup to get React working in
  Vite" on the fourth. That text describes the generator, not the repository.
* `qurio-toolset`'s root was then described as "a DEV-ONLY Vite plugin", because
  the doc-comment extractor fell back to the district's monument wherever it sat
  in the subtree — here `vite-plugins/manualReloadPlugin.js`. The same rule made
  `components/MediaWindow` a YouTube renderer, from
  `MediaWindow/renderers/YouTubeRenderer.jsx`.
* `components/ChatWindow` was described as "most imported: `ChatWindow.jsx`" and
  `src/types` as "most imported: `index.ts`". ADR-0087's `says_nothing_new`
  guard missed both: it compares against the *display name*
  (`components/ChatWindow`), which a leaf file name never equals, and it does not
  strip the extension.

**Decision.** Four rules, all of which make the output *smaller*.

1. **`SanitiseReject::Boilerplate`.** Two closed tables — `PRAGMA_PREFIXES`
   (`eslint-*`, `prettier-ignore`, `ts-nocheck`, `noqa`, `pylint:`, `coding:`, …)
   prefix-matched after markup stripping, and `BOILERPLATE_PHRASES`, verbatim
   generator output, matched anywhere. Rejected whole and the next source tried,
   the same contract as a secret. A phrase earns a place in the second table only
   by being a string a tool emits, never by sounding generic: the general "says
   nothing new" test is `says_nothing_new`, and the general "there is nothing to
   say" answer is `None`.
2. **The cache version is bumped with any rule change.** The cache is keyed on
   the bytes of the file, not on the rules that read them, so a warm cache would
   go on serving the answer the new rule exists to refuse — and only on machines
   that had run before, which is the worst possible way to find out.
3. **An `extra` doc-comment candidate must sit in the directory itself.** A doc
   comment describes the file it is written in. An `ANCHOR_NAMES` file is the
   directory's declared front door and may speak for it; any other file speaks by
   proximity, and proximity runs out at the first subdirectory. This does not
   make the survivors true of a whole 223-file district — that is a judgement no
   path rule can make — it removes the cases with no basis at all.
4. **A monument that repeats the district's own leaf name, extension stripped,
   is not a description**, and neither is a universal entry point (`index`,
   `main`, `mod`, `lib`, `app`, `__init__`, …). The monument itself is untouched:
   PRD §8 still draws and labels that building. It is a useless label for the
   *district*.

Also: `.astro`, `.playwright-mcp`, `playwright-report`, `test-results`,
`.docusaurus`, `.vercel`, `.netlify` and `.wrangler` join
`DEFAULT_INDUSTRIAL_DIRS`. All are tool output that nobody edits, and without
them 152 such files sat in the civic budget of the operator's repositories —
106 of them, in `biwt`, as one of that repository's largest `config` districts.

**Consequences.** Prose descriptions across the operator's eight repositories
fall from 44 to 38 and the *informative* count stays at 10, which is the point:
every loss was wrong. The honest number is now 23 prose descriptions across 160
districts outside this repository — 14 % — of which 10 tell the reader something
the label did not. `docs/design/NEIGHBORHOODS-REVIEW.md` records the grading
district by district, and concludes that the remaining gap is not reachable by
more rules.

**One thing deliberately not fixed.** `vc-tower/vcsheet-scraper` holds 13 058
scraped `.json` and `.html` files that read as `config` and `source`, set the
sizing budget, and collapse that repository's whole application into one
100-file district. `.json → Config` was chosen deliberately in ADR-0085 and
flipping it globally is not a review's change to make. The right fix is to
generalise ADR-0086's hold-out from *directory names* to *behaviour* — a flat
directory of thousands of files sharing one extension is a mass whatever it is
called — and it is the top recommendation of the review.

---

## ADR-0089 — A model writes the descriptions the repository does not, and everything about it is fenced

**Status** accepted · Implemented in `polis_repo::llm`.

**Context.** ADR-0087 derived a neighborhood's description by quoting the
repository, and ADR-0088 tightened it. `docs/design/NEIGHBORHOODS-REVIEW.md` then
measured the result by running it over the operator's eight repositories:
**23 prose descriptions across 160 districts — 14 % — of which 10 told the reader
something the label did not.** Nine of the 23 were the folder name in different
words (`services/settings` → "The settings entry contract"). `biwt` has 20
districts and zero prose in the whole checkout.

The gap is structural, not a missing rule: **most directories in a working
repository contain no sentence saying what they are**, and no extractor can quote
what nobody wrote. So the operator approved adding a model, after an explicit
discussion of the trade-offs, and PRD §2's "no telemetry leaving the box" is bent
here — deliberately, once, and with the payload fenced.

**Decision.** `GLM-5.3-Flash` on Z.ai by default, behind a provider-agnostic
interface, with seven properties that are each load-bearing.

**1. The provider is a config line.** `ChatProvider` has two methods: build a
request, parse a response. Everything above it — planning, batching, caching,
staleness, redaction, retries, accounting — is written against that. `Glm`,
`Ollama` (the same `OpenAI`-compatible shape, locally, with no key at all) and
`Anthropic` (Messages API, `x-api-key`, `anthropic-version`) ship.
`LlmConfig::with_provider` moves base URL, model, key-variable *name* and price
together, because changing one without the others is the failure it exists to
prevent.

**2. There is no HTTP crate.** A blocking client with TLS is ~40 crates including
a cryptography library with a C build and a compiled-in CA set that ages;
`polis-repo` has nine direct dependencies and this work added **zero** —
`Cargo.lock`'s `polis-repo` entry is untouched, so a security review of the
feature is still one directory of one crate, and `polis-hook`'s zero-dependency
guarantee (PRD §14, ADR-0036) cannot be eroded by accident. Instead there are two
transports behind one trait: `http://` is HTTP/1.1 over `std::net::TcpStream`,
written out here (chunked decoding included, because Ollama uses it); `https://`
is `curl` as a subprocess, using the operating system's own trust store. This is
the same call `polis-repo/Cargo.toml` already made for `git log`, for the same
reasons. **It is a trait so it is reversible**: a `ureq`-backed
`impl Transport` is a new type in the caller's crate and no change here.

A consequence worth stating: the tests drive the *shipped* plain-HTTP client
against a real `TcpListener` on `127.0.0.1`, so the end-to-end HTTP path is
exercised by `cargo test` with no network and no mock.

**3. The key is a type, not a discipline.** `Secret` has no `Serialize`, no
`Display` and no constructor from a literal; it prints as `Secret(<redacted>)`,
so a `{:?}` of any struct holding one is safe to log; it is reachable only
through `Secret::expose`, which is one greppable name. `curl`'s command line is
visible to every process on the machine, so a secret header is written to
`curl --config -` **on stdin** and never into `argv` and never into a file; the
request body, which carries no key, goes to a temporary file in the state
directory that is deleted when the call returns. Any text arriving from outside
this crate — a subprocess's stderr — is run through `secret::scrub` before it can
reach an error string.

**4. Editing a file does not change what a folder is.** The cache key is the
district's **identity plus a fingerprint of its file names**, never file
contents. Keyed on content, every commit invalidates most of the cache: a
one-time cost becomes a per-commit cost and, worse, the words on the map churn —
which is the same failure as a city that reshuffles (PRD §7.4). A description is
regenerated only when meaning plausibly moved, which is three things:

* **a district appeared** (`Freshness::Missing`);
* **a district split or merged** — the entry stores its child district paths, so
  a growing `src/services` splitting into `src/services/auth` and
  `src/services/billing` is caught *structurally*, even though the parent's own
  file names barely moved. This is the case the operator asked about and the one
  a name fingerprint alone misses;
* **the name fingerprint drifted** past the threshold.

Relations are deliberately not on that list. Import edges are exact and update
instantly; they answer "what talks to what". Descriptions answer "what is this
folder", which moves slowly. Keeping them apart is what stops the fast-changing
half from ever triggering a call.

**The threshold is 440 ‰ of Jaccard distance, and the number is measured, not
chosen.** `polis-repo/examples/llm_drift.rs` lists every district of every
repository in `C:/coding` at `HEAD` and at the same repository 30, 90 and 365
days — and 25, 100 and 400 commits — earlier, using `git ls-tree` so nothing is
checked out. Drift is `1 - |A ∩ B| / |A ∪ B|`, symmetric in additions and
removals, which is the property you want: a district that *loses* a third of its
files has changed as much as one that gains a third. That symmetry is also why
the constant is not the brief's "a third" read literally — replacing a third of
*n* names gives 500 ‰, adding a third gives 250 ‰, so "a third changed" is a band
from 250 to 500 and the measurement picks the point inside it:

| window | surviving districts | ≥250 ‰ | ≥330 ‰ | **≥440 ‰** | ≥500 ‰ | ≥660 ‰ |
|---:|---:|---:|---:|---:|---:|---:|
| 30 days | 117 | 11 % | 7 % | **3 %** | 3 % | 2 % |
| 90 days | 113 | 19 % | 12 % | **9 %** | 5 % | 4 % |
| 25 commits | 110 | 15 % | 11 % | **9 %** | 9 % | 7 % |
| 100 commits | 83 | 43 % | 33 % | **25 %** | 23 % | 13 % |
| 400 commits | 55 | 60 % | 56 % | **44 %** | 40 % | 29 % |

At 440 ‰ the *median* district never fires in any calendar window — median drift
is 0 ‰ on five of the eight repositories over 90 days — so the typical caption is
stable across a quarter, which is exactly the spatial-memory property. It still
fires on the districts that genuinely churned: `qurio-toolset`'s 90-day p90 is
784 ‰. 330 ‰ costs a third more regenerations for districts whose names moved by
a quarter, which is a refactor, not a change of purpose; 250 ‰ nearly doubles it.

**5. Staleness is a state, never a silence.** A caption reading "Payment
processing" over a folder that quietly became the notification service is *worse
than no caption*: it is confidently wrong and the operator would trust it. So
`Neighborhood::freshness` is `Fresh` / `Stale(Drift { permille, cause })` /
`Missing`, the cause distinguishes names from split, merge and a changed
model-or-prompt, and the drift is a `u16` per-mille rather than an `f32` because
`Neighborhood` derives `Eq` and a serialized value must compare bit-identically
on every machine. A stale description is **still shown**, marked — blanking it
would trade a caption the operator can see is old for no caption at all, and the
requirement was that staleness be visible, not hidden.

**6. Nothing is called implicitly, and every failure degrades.**
`RepoIndex::neighborhoods_described` and `llm::apply_cached_model_descriptions`
read; only `LlmRunner::run` calls, and `LlmRunner::spawn` puts it on a background
thread so a render never waits for a network. **`RunMode::DryRun` is the
default**, produces the whole priced plan having called nothing, and a
`Generate` against a **cold cache** refuses to spend unless confirmed: cold start
is the expensive one and nobody should discover a bill. No key, no network, a
dead port, a 401, a 429, a timeout, a body that is not JSON, a model that
refuses — each is one line in `RunReport::errors` and a map identical to the one
you get with the feature off. Retries are bounded and only for errors that could
change: a 429 and a 5xx are the endpoint asking for patience, a 401 will be just
as wrong in half a second.

**7. Empty beats filler, enforced twice.** The prompt leads with the refusal
rule, in capitals, illustrated with three of the review's *real* failures
verbatim, and asks for `null`. And `prompt::restates_the_name` enforces it
independently, because a rule that lives only in a prompt holds only until the
next model: caption words are split on `camelCase` and punctuation, singularised,
and stripped of stopwords and a closed list of category nouns; if nothing is left
that the district's name does not already contain, the answer is refused. Graded
against the review's own tables it catches **six of the nine** restatements and
**none of the ten** informative captions — "Tool Registry" survives, because
`registry` is neither in `services/tools` nor a category noun. It is deliberately
high-precision: a false positive here silently deletes a good caption.

The same function also decides *which* districts are asked about. A real README
or manifest sentence always wins; the model is asked only where the extractor
returned nothing, returned an inventory line, restated the name, or hung one
file's doc comment on a district too large for it to speak for
(`src/services`, 223 files, labelled from `WindowManager.js`).

**Consequences.**

*Nothing leaves without being vetted.* `llm::outbound::vet` applies
`describe::looks_like_secret` — the **same** predicate as the inbound sanitiser,
not a second copy — to every file name and doc snippet, strips every control
character and bidi override with `describe::is_unsafe_char`, and drops whole
fields rather than redacting inside them. Measured over the operator's eight
repositories: **169 names refused, 0 doc snippets, 0 districts skipped.** One of
those is a genuine catch — `AuthKey_73425WW27A.p8`, the Apple private key in
`qurio-toolset/electron/signing/apple`, the district ADR-0087's review already
flagged. The rest are long hyphenated document names with digits
(`1968-End-of-an-Era.pdf`) and epoch-prefixed uploads
(`1765241215200-A Cielo Abierto.png`) that read as high-entropy. That is the
right side to err on, and it is why vetting happens *before* the sixty-name
truncation rather than after: otherwise `stickingplacebooks`' upload directory
would have spent its whole budget on names that were then dropped.

*Source code bodies never leave.* Structurally, not by promise: `DistrictBrief`
has no field that could hold one and its builder opens no file.

*The layout cannot move.* A description is text hung on a district. The partition
is a pure function of the file list and runs before any of this; there is a test
that a full generation leaves every district's path, file count and kind
byte-identical, and the `m1_gate` goldens are untouched.

*`DescriptionSource::Model` is not prose.* `is_prose()` stays false for it and
`NeighborhoodStats` counts `described_model` separately, so the review's 14 % —
the honest measurement of how well a repository documents *itself* — cannot be
inflated by having paid for sentences.

*Cost, measured rather than estimated.* A cold start over the operator's eight
repositories, priced from the exact bytes that would be sent: `qurio-toolset`
$0.0018 (41 districts, 6 calls), `stickingplacebooks` $0.0013,
`Squigglo` $0.0008, `biwt` $0.0006, `stickingplace` $0.0004, `vc-tower` $0.0003,
`agentolis` $0.0001, `qurio-networked` $0.0001 — **$0.0054 for all eight, about
$0.0007 each**, an order of magnitude below the $0.006 per repository the brief
estimated, because a district's names and doc snippet are ~250 input tokens, not
1 000. The price is configuration, not a constant, because the shipped default is
a promotional rate that expires on 2026-09-09, and `RunReport` prints the price
it used and says plainly when the provider reported no usage and the figure is
therefore an estimate.

*Structured output is requested and never relied on.* `models.dev` reports that
`glm-5.3-flash` supports it; Z.ai's documentation does not describe the syntax,
and this round had no key to settle it with. So `response_format` is sent when
`request_json_object` is set — one config line to turn off — and `parse_reply` is
tolerant either way: a bare object, a bare array, a fenced block, or JSON inside
prose. **This is unverified**, and it is written down here rather than assumed.

*What the live endpoint did confirm, with no valid key.* An unauthenticated
`POST https://api.z.ai/api/paas/v4/chat/completions` returns HTTP 401 and
`{"error":{"code":"1001","message":"Authentication parameter not received in
Header, unable to authenticate"}}`; the same request through the shipped
`CurlTransport` carrying a deliberately invalid key returns HTTP 401 and
`"token expired or incorrect"`. The two messages differ, which is the proof that
the `--config -` stdin mechanism actually delivers the header. The run classified
it as non-retryable, made one attempt, logged one error line and left both
districts on their derived descriptions. **A successful completion has not been
observed**: no key was present in this environment.

---

## ADR-0090 — A claim is keyed by actor, because every real contention is inside one session

**Context.** PRD §11.3 says to register an `Edit`/`Write` claim "keyed by
**thread**", and PRD §3 defines a thread as a main agent *plus its worker
subtree*. Those two sentences together make contention unrepresentable in the
case it actually occurs: a session that fans out to a dozen parallel subagents is
a whole fleet inside one `ThreadId`, so "a second live claim from a different
thread" can never fire however many workers collide.

**Decision.** Key the claim on `Actor = (thread, worker)`. A hit is a second live
claim from a different **actor**, and `Contention::within_one_thread()` reports
whether both ends belong to one session so a surface can word it correctly.

**Evidence, on the operator's own corpus.** `contention_on_real_sessions.rs`
replays two real sessions and asserts the result:

| session | events | contentions | class |
|---|---:|---:|---|
| `settings.css` session | 4 604 | 1 | `worker a6bb0ab5e…` vs `worker a79d04997…` on `src/components/custom/settings/settings.css`, High / FileLevel |
| 69-worker fan-out | 37 687 | 3 | `polis-render/src/live.rs`, `polis-world/src/apply.rs`, `polis-app/src/cli.rs`, each worker-vs-worker |

**Four of four real contentions are worker-versus-worker inside a single
session.** A `ThreadId`-keyed table represents none of them; the number it
reports on this corpus is `0`. Contention is PRD §11.1's top-ranked state — the
only one where work is actively being destroyed — so a table that cannot
represent the common case is not a conservative choice, it is a silent one.

**Consequence we are accepting.** `edits_without_line_ranges` is 1 768 against 8
hits, so PRD §11.3's **Critical** tier ("same branch, overlapping line ranges")
is effectively unreachable from the transcript and every file-level hit lands at
**High**. That is reported in `Health` rather than papered over, and it means the
severity ladder currently has three usable rungs, not four.

---

## ADR-0091 — The alarm is area and motion, not a redder red

**Context.** By M4 the colour channel was correct — every failure placed, drawn
in `AGENT_FAILED` at the mark — and worth nothing from a metre away. Measured:
the reddest frame in 1 440 held **142 red pixels out of 1.21 M**, two glyph
outlines. An independent review: *"a failing session and a clean one are
distinguishable in about a second by reading one number, and not distinguishable
by peripheral vision, which is what PRD §11.4 actually asks for."*

**Decision.** Treat it as an **area and motion-onset** problem, exactly as PRD
§11.4 frames it (*"peripheral vision is poor at colour and good at motion
onset"*, *"colour alone is never the sole channel"*). Cluster nearby failures
into one **alarm** per region and draw three things: an expanding arrival ring
gone in ≤400 ms, a heavy broken steady-state ring with radial ticks, and
persistence that never fades below `ALARM_FLOOR`.

**Measured on a real session** (`29c2fc6f`, 14 979 events, 120 frames, 68 of them
carrying an alarm), reddest frame #102, 60×60 thumbnail:

| notation | peak red-excess | hot px of 3 600 | share |
|---|---:|---:|---:|
| M4, no alarm | 42 | 3 | 0.08 % |
| **M5, alarm** | **70** | **42** | **1.17 %** |

Fourteen times the area. Making the red redder would have bought nothing and
would have broken PRD §10.3's band scheme; the alarm is drawn at the ceiling of
`AGENT_BAND` (channel 168 against a base map clamped at 48) rather than being
promoted into layer 5, which PRD §11.2 reserves for exactly three states.

**Bounded on purpose,** because PRD §17's default failure mode is a map that
panics: one ring per region, `ALARM_CAP` of eight worst-by-count, and
`ALARM_MAP_CAP` so no ring exceeds 9 % of the map.

**Honest limit.** 1.17 % of the thumbnail is fourteen times better and is still
about one part in a hundred. Whether that clears "readable in peripheral vision"
is a human judgement this project has not run a human trial on. The channel PRD
§11.4 actually names — motion onset — is implemented and asserted
(`an_arriving_pin_pulses_outward_and_is_gone_in_four_hundred_milliseconds`: 48 px
at rest, 73 px mid-pulse) and cannot be measured from a still frame at all.

---

## ADR-0092 — `TurnEnded` must not fire for a headless session, and the transcript already says which is which

**Status: open defect, not yet fixed. Found by the M3/M4/M5 end-to-end
verification.**

**Context.** `DecisionSource::TurnEnded` raises *needs decision* when a main agent
ends its turn with no tool call and no human has replied yet. It is the most
common source by far — 62 / 38 / 6 occurrences in the three recorded sessions,
median waits of 5, 13 and 27 minutes — and it is the reason a replay can show
PRD §11.2's primary state at all. It is also correct: for an interactive session,
"turn ended, nobody has answered" *is* the operator being waited on.

**The defect.** For a headless `claude -p` session it is never correct. The
process has exited; no human is going to reply; the mark can never resolve. And
`World::retire_threads` deliberately never retires a thread that an attention
mark still points at — rightly, because a thread blocked on a human may sit for
hours and that *is* the product. The two rules compose into an unbounded pile of
false alarms.

**Observed.** Twelve headless sessions run in one scratch checkout: twelve threads
on the map, twelve amber `WAITING ON YOU` pins, every one of them for a process
that had already exited, ageing past three minutes and climbing. That is PRD
§17's *"beautiful swarm view that makes the operator feel informed while telling
them nothing actionable"* arriving through a door we built.

**The fix, and why it is cheap.** Every threaded transcript record carries
`entrypoint`. `docs/verified/jsonl-schema.md` records it as **STABLE 100 %** with
values `"cli"` (125 553) and `"sdk-cli"` (120), and
`polis-ingest/src/transcript.rs:182` already parses it into
`TranscriptRecord::entrypoint`. **No code in `polis-world` reads it.** Gate
`TurnEnded` on an interactive entrypoint and the false pins disappear; the other
six `DecisionSource` variants are unaffected, because each of them is evidence of
a question that was actually asked.

**Why it was not fixed in this change.** The verification pass owns `README.md`
and this file; `polis-world` belongs to the milestone that will carry the fix.

**Blast radius.** Negligible for an operator typing at a terminal — `sdk-cli` is
0.1 % of the real corpus. Total for anyone driving a fleet with `claude -p`,
which is the shape PRD §16's synthetic load ("100 threads × 400 subagents")
assumes and the shape a scripted fleet actually has.

---

## ADR-0093 — The cold-start budget is a per-parseable-file cost, and ADR-0082's Django row is not representative

**Context.** ADR-0082 concluded that "a genuine 5 000-file repository is inside"
PRD §13.1's 3-second cold-start budget, resting on Django: 7 014 files, cold
2 998 ms, of which imports 423 ms. The end-to-end verification could not
reproduce that ratio anywhere.

**Measured.** Five thousand and forty files, one language each, nothing cached,
release build, this machine:

| all 5 040 files are | walk | history | imports | diff | layout | render | **cold** |
|---|---:|---:|---:|---:|---:|---:|---:|
| `.py` | 16 | 75 | 2 896 | 68 | 263 | 606 | **3 923** |
| `.ts` | 15 | 69 | 3 945 | 69 | 250 | 589 | **4 936** |
| `.rs` | 15 | 59 | 4 915 | 73 | 264 | 608 | **5 933** |
| `.rs`, zero `use` statements | 14 | 68 | 5 161 | 67 | 248 | 597 | **6 156** |
| `.js` | 15 | 70 | 6 557 | 66 | 255 | 631 | **7 595** |

**Every grammar Polis ships misses the budget at 5 000 parseable files.**
Stripping every import statement made it slightly *worse*, so the cost is
per-file parsing, not edge resolution.

**The rate is 0.86–1.30 ms per parseable file,** and it is corroborated outside
the synthetic set: `qurio-toolset`, a real checkout of 1 557 files, spends
1 340 ms in imports on a cold cache — 0.86 ms/file, which extrapolates to ~4.3 s
at 5 000.

**Therefore ADR-0082's Django row is an artefact of file mix, not a refutation.**
423 ms over 7 014 files is 0.06 ms/file, fourteen times below every other
measurement taken here. Django's tree is largely `.html`, `.po`, migrations and
static assets; most of those files were never handed to a grammar. ADR-0082's
conclusion should be read as *"a repository with a typical mix of parseable and
non-parseable files is inside the budget"*, which is true, and not as *"5 000
files is inside the budget"*, which is not.

**Decision.** Record the real curve and leave the budget missed rather than
restate it. The mitigation that already exists is that the import cache is keyed
on **content, not path**, so only the first sight of a given tree pays: every
later launch of the same 5 040-file repository is **816 ms**. What is not yet
built is anything that makes the *first* launch honest — a progress indication, a
first frame that draws before imports finish, or a parse budget that yields.
`polis snapshot` currently prints the budget beside the measurement and says
nothing when it is over.

---

## ADR-0094 — What the M3/M4/M5 verification could not verify

**Context.** A verification that only reports what passed is an advertisement.
These are the things this round tried to establish and could not, recorded so the
next round does not have to rediscover them.

**Hooks were never observed firing from a real agent on this machine.** Claude
Code runs no settings-file hook until the workspace trust dialog has been
accepted, and a scratch checkout created by an agent has not been. The remedy
Claude Code itself prints is to set `hasTrustDialogAccepted` in the operator's
`~/.claude.json`, which a verification agent must not do on the operator's
behalf. Channel B was therefore driven through the **real `polis-hook` binary**
over the real wire instead — contention, `PermissionRequest` and `Stop` all
arrived and rendered — which tests the transport and the world but not Claude
Code's own invocation of the hook. `polis install-hooks` already warns about this
exact trap at install time.

**`cargo doc` is not clean, and was not clean before this round.** Ten warnings,
all in `polis-repo` (`llm/prompt.rs`, `describe.rs`, `imports.rs`), all public
docs linking to private items, plus two unresolved `Describer::describe` links.
`polis-repo` has no uncommitted changes, so these arrive from the committed LLM
work rather than from M3-M5. They are ten one-line doc-link fixes and are left
for whoever owns that crate.

**The two-machine determinism leg is still unobserved.** Byte-identical across
runs, processes, input permutation, `RandomState` order and now **optimization
profiles** — the fixture digests are `hamlet=23203396b91ae4bc` and
`town=22d10d8e6e4762df` in debug and in release — but this remains one machine.

**Port conflicts recover, with one rough edge.** A foreign listener on 4317 makes
Channel A stop with an exact message, emit `control.degraded`, and leave the
other three channels running; `--allow-second` behaves exactly as documented
(warns, skips Channel B, does not clobber the primary's endpoint file). The rough
edge: for a few hundred milliseconds after a Polis is killed, Windows has not yet
released the UDP socket, so the next Polis reports "another polis is already
receiving hook events" and names a pid that has just died. It self-heals and no
operator action is needed, but the message is briefly wrong.

**A live watch holds a handle on the directory it watches.** Deleting the watched
repository out from under a running window removed every file including `.git`
and Polis survived with no panic and no hang — but the now-empty top-level
directory could not be removed until the window was closed. That is the `notify`
watcher's handle, and on Windows it means "close Polis before you `rm -rf` the
checkout".

---

## ADR-0095 — Polis grows a terminal, and the terminal is a server

**Context.** `polis-app/src/run.rs` gave three reasons for launching an agent as a
foreground child that inherits the real console, and named the alternative in the
same breath: *"anything else is a pty emulation Polis has no reason to write."*
All three are re-examined here rather than dismissed.

1. *"A pty emulation Polis has no reason to write."* Satisfied by `ConPTY`
   without writing one. `alacritty_terminal` **is** that code — the same crate
   Alacritty itself ships — and nobody should write a VTE state machine twice.
2. *"The exit code is the agent's."* Does not apply to a window, which has no
   exit code to donate. `polis run` still does, and is untouched.
3. *"The window prints to stderr into the agent's UI."* Is *fixed* by a pane, not
   caused by one.

What changed is the requirement, not the reasoning: `polis run` watches **one**
agent, and PRD §11's thesis is **several**. One inherited console cannot be
several agents.

`docs/roadmap/terminal-integration.md` planned this as M7 — ptys inside the
window — with a session daemon deferred to M8 as "later work".

**Decision.** Build the terminal, and put the ptys in **`polis-sessiond` from the
first commit**. The window is a thin client that owns the parser, the grid and
the keyboard, and owns no child process at all. Three reasons, in order of
weight:

1. **A window that owns agents kills them when it closes.** A GPU driver reset is
   not rare on Windows and would take an afternoon's work with it. tmux (2007),
   herdr and cmux each reached the same split independently.
2. **The pty master is not blocking-readable on Windows.** Measured, and it is
   the finding that inverted the plan. The roadmap's reader loop —
   `match reader.read(&mut buf) { Ok(0) | Err(_) => break, … }` — **exits
   immediately, having read nothing: `Ok(0)` after 6.8 µs, zero bytes.**
   `alacritty_terminal`'s Windows master is an `UnblockedReader` whose `Read`
   impl is a non-blocking drain of an internal `piper` pipe; `Ok(0)` means
   "nothing right now", and end of file arrives separately through
   `EventedPty::next_child_event`. Readiness comes from a `polling::Poller` and
   from nowhere else — the same spike against a poller read 89 bytes of real
   `ConPTY` output in two reads and saw `Exited(ExitStatus(0))` at 13.4 ms.
   A process whose main thread belongs to winit has nowhere natural to put that
   loop. **A daemon is one.** The inversion made the hard part easier.
3. Doing it later means doing it twice.

**The one `unsafe` in Polis.** `EventedReadWrite::register` is an `unsafe fn`,
because on Unix it lends a file descriptor to the poller. The roadmap asserted
that neither pty option "forces `unsafe` into our crates"; that is false for this
one. It is discharged structurally — `PtyHost` owns the `Pty` and the
`Arc<Poller>` together, both move into the same thread, and the `Pty` is dropped
inside that thread before it returns — and it is one `#[allow(unsafe_code)]` on
one call with the argument written above it, not a block of raw FFI. The
alternative, `portable-pty`, needs no poller and costs roughly ten crates on a
legacy `winapi` 0.3 stack in the one process that must never be flaky.

**Dependency cost, measured against the existing lock: nine crates.**
`alacritty_terminal 0.26.0`, and with it `vte`, `home`, `miow`, `piper`,
`futures-io`, and on Unix `rustix-openpty` and the two `signal-hook` crates.
`windows-sys` 0.61.2, `polling` 3.11.0, `base64`, `parking_lot`,
`regex-automata`, `unicode-width` and `serde` were already there — in particular
there is **no second `windows-sys` generation**. `vte` arrives re-exported as
`alacritty_terminal::vte` and must never be pinned directly, which is the same
rule the root manifest already applies to `wgpu` and `winit`.

**Consequences.** Two new members. `polis-term` holds the pty, the wire, the
parser, the widget, the key table and the font chain, with the egui half behind a
`ui` feature so `polis-sessiond` takes it with `default-features = false` and
has no eframe in its graph at all. `polis-app` gains `panes.rs` and
`Mode::Work`, and `polis work` joins the CLI.

`polis run` is **untouched**: inherited stdio, foreground child, the agent's exit
code, and its own careful four-step teardown. If Polis ever routes `polis run`
through a pty that is a separate decision and a separate ADR.

The window's terminal teardown, by contrast, is `drop(client)`. It never owned a
child, so there is nothing to kill — which is the whole point, and is verified:
a Polis window force-killed with `Stop-Process -Force` left its `claude` running,
and the next `polis work` reattached to it and put the screen back.

---

## ADR-0096 — The pane's session id is issued, not inferred

**Context.** A pane and a cloud on the map have to be the same thing, or the
feature is a terminal bolted onto a map. The join is a session id, and there are
three ways to get one: infer it, thread it through the hook, or issue it.

**Decision.** Polis generates a v4-shaped uuid **before** spawning and passes
`claude --session-id <uuid>`. Measured against `claude --help` 2.1.248: the flag
exists and is **not** gated on `--print`. Every channel then carries it for free —
the OTLP resource attribute, the `session_id` in every hook payload, and the
transcript's own filename, `~/.claude/projects/<slug>/<uuid>.jsonl`. Nothing is
inferred, nothing is timed, nothing is raced.

The uuid is ~15 lines from `(process id, pane ordinal, wall clock nanoseconds)`
with the version nibble and variant bits set, rather than a `uuid` dependency:
the requirement is *unique on this machine*, not *unpredictable anywhere*, and
`uuid` is presently only transitive. This is the same call that wrote out simplex
noise rather than take `noise` (ADR-0050). Claude Code validates the *shape*, so
the shape is asserted in `proto.rs`'s tests.

**Rejected: threading a pane id through `polis-hook`.** Tempting, because the
hook already reads one environment variable and one more `var_os` is
sub-microsecond against a 3 ms p99 budget. **The env read is not the cost.** The
hook ships an 8-byte header and opaque bytes, so a pane id goes *into the wire
format*: a new field, a version bump, a matching `hook_listener` change and a
re-derived compile-time assertion — to the one binary whose contract is "never
blocks the agent, never exits non-zero, no dependencies beyond `std`" — in
exchange for what `--session-id` gives free. `polis-hook` is untouched.

**Rejected: a hook listener port per pane.** Needs zero hook changes and is
elegant, but needs N sockets and N threads in `Ingest` and multiplies the
`AddrInUse` singleton logic ADR-0026 established, for nothing over
`--session-id`. It is the fallback if the flag is ever removed.

**Consequences.** `Dock::pane_for_session` is a lookup, so clicking a cloud can
focus its terminal in one line, and a tab can name the district its agent is
working in. A pane running something that is not `claude` gets **no** session id
and says so, rather than guessing: a false positive points a cloud at the wrong
terminal, which is worse than pointing at none.

---

## ADR-0097 — Ctrl+C reaches the agent, `⎿` is drawn, and `has_glyph` cannot be trusted to tell you

**Context.** Three findings that look cosmetic and are not. All three were
measured; the third was measured *wrongly first*, which is the most useful part
of this record.

**One: `egui-winit` never emits Ctrl+C.** Verified at
`egui-winit-0.36.1/src/lib.rs:1021-1035` — `is_copy_command` pushes
`Event::Copy` and **returns**, so `Event::Key { C, ctrl }` is never produced.
Same for Ctrl+X and Ctrl+V. Ctrl+C is the key that interrupts Claude Code, so
untreated **the operator cannot stop a runaway agent from inside Polis**. That is
a safety property, not a convenience.

*Fix:* `eframe::App::raw_input_hook` (`eframe-0.36.1/src/epi.rs:279`) runs before
egui processes a frame's input. When a pane has focus and no selection,
`Event::Copy` is rewritten back into the key event — the Windows Terminal rule,
copy when there is a selection and interrupt when there is not. `Event::Paste` is
left alone, because pasting is what Ctrl+V means. `tests/keys.rs` asserts
`\x03`.

**Two: `Ctrl+Alt` must never become a control byte.** On a German, French or
Polish layout **`AltGr` is reported as Ctrl+Alt**, and `AltGr+Q` is how you type
`@`. winit delivers the `@` as `Event::Text` and *also* delivers
`Event::Key { Q, ctrl + alt }`; encoding that as Ctrl+Q would send `@` followed
by `\x11` into an agent's input box every time somebody typed an email address.
The combination therefore produces nothing — which is also what makes `Ctrl+Alt`
safe for the dock's own chords.

**Three: seven glyphs, and an oracle that lies about them.** Claude Code draws
`⎿` at the head of every tool line and cycles `✻ ✽ ✢` as its spinner. Measured
through the renderer that will actually draw them, **eframe's four bundled fonts
cover 8 of the 16 glyphs Claude Code is known to use** — `⎿ ✻ ✽ ✢ ✓ ✗` and the
braille cells are missing; box-drawing and blocks are present, in Hack. Adding
the operating system's own `seguisym.ttf` — Segoe UI Symbol, on every Windows
since 7 — makes it **16 of 16**. Cost: zero bytes of binary and no licence
question, because Polis *reads* a font the OS installed and redistributes
nothing. Bundling Cascadia Mono instead would cost 363 KiB and still miss `⎿`,
`✻` and `✗`.

*And the trap.* `epaint 0.36.1` implements `Fonts::has_glyph` as
`resolve_face(c) != cached_family.replacement_face_key` — "is this character
served by a different face than `U+FFFD` is?" That is a **false negative for
every glyph living in the same face as the replacement character**, which is
normally the first font in the family; asked about a single-font family it
reports that nothing at all is covered. Measured that way, the bundled chain
appears to be missing `─ │ ╭ █ ░ ▶` as well, and this project spent a round
designing a three-font fallback for a problem that did not exist before the
oracle was replaced with "lay the character out and compare the atlas rectangle
it got against the one `U+FFFD` gets". The roadmap's original `cmap`-parsing
measurement was right all along.

**Consequences.** `polis_term::font::coverage_line` prints
`terminal glyphs  16/16 (seguisym.ttf)`, or names exactly which are missing and
what each is for — a cosmetic mystery turned into a one-line diagnosis. On Linux
none of the candidates is guaranteed present, so `install` degrades to
replacement characters and says so; CI's ubuntu leg asserts that it degrades
without panicking, not that coverage holds.

---

## ADR-0098 — The boundary is bytes, the transport is a token on loopback, and the daemon must forget its parent

**Context.** Three decisions about the wire between `polis-sessiond` and the
window, each of which had an obvious answer that is wrong.

**Bytes, not screens.** The obvious design serialises a styled 45×120 grid per
pane at 30 Hz. That is the design tmux spent years adding flow control to
survive, and tmux's own control mode does not do it either: `%output %pane
<data>` ships raw bytes and the client parses them, which is how iTerm2 renders
tmux panes as native tabs. Shipping bytes is the single decision that keeps this
cheap — the VT state machine, the grid and the widget all live on the window's
side, and the daemon never learns what a cursor is. It also makes backpressure
free: each subscriber tracks how far into each pane's byte log it has been sent,
and a client whose queue is full is simply not sent to, so the next event
coalesces the gap into one larger message. That is the outcome `pause-after`
buys tmux, without the protocol.

**Loopback TCP with a token, not a named pipe.** The roadmap planned a named pipe
on Windows and a Unix socket elsewhere, and called it "the only genuinely
platform-forked code in M8". There is none: `std` has no named-pipe API, so
`CreateNamedPipeW` means either raw FFI — which `unsafe_code = "deny"` rules out,
and which `polis_ingest::hook_listener` already refused for the same reason — or
a Windows-only crate whose Unix twin is a second code path. Polis already binds
fixed loopback ports (ADR-0026); this is the third.

A loopback port carries no ACL, and *this* one spawns processes on request, so it
is a meaningfully worse thing to leave open than an event receiver. The daemon
therefore writes 256 bits of hex — seeded from the OS's own randomness through
`RandomState` — into a file only this user can read, and serves no call before
`hello` presents it, compared in constant time. Same shape as Jupyter's, for the
same reason, and honest about what it defends: a process running **as this user**
can read the token file, and could equally read `~/.claude` directly. The fence
is against other users and other origins, not against the operator.

**A daemon must forget its parent.** Found by running the end-to-end pane test
from inside a Claude Code session: the pane worked, and the screen said
*"Transcript saving is off — inherited `CLAUDE_CODE_CHILD_SESSION` marker"*.
Transcript saving is **Channel D**, the one channel that needs no hooks and no
environment, the one `polis watch` is built on, and the one carrying the
session's own uuid in its filename. A daemon launched from inside an agent would
have silently disabled it for every agent it went on to start, and the map would
have been permanently short a channel for a reason nothing pointed at.

`polis_term::pty::INHERITED_AGENT_MARKERS` names nine session-identity and IPC
variables, each with its reason, and the daemon clears them from its own
environment before starting any thread — which is the only place it works, since
a pty's options can add to a child's environment but cannot take anything away.
It is a **list and not a prefix rule**: `CLAUDE_CODE_USE_BEDROCK` and
`CLAUDE_CODE_MAX_OUTPUT_TOKENS` are configuration an operator set on purpose, and
a prefix rule would throw those away along with the identity.

**Consequences.** PRD §2's non-goal says "no server", and this is a local
background process. The clause means **no cloud, no auth, and nothing leaving the
box** — all three of which still hold: `bind` refuses anything but `127.0.0.1`,
there is no account, and nothing is ever sent anywhere. §2 says the shorter thing,
so it is amended to say the longer one rather than quietly reinterpreted.

The orphaned-daemon failure mode gets three answers: `--idle-timeout` (default
600 s) exits a daemon holding **no panes and no clients**, and never one holding
a live agent; `polis-sessiond --status` says what is running from any terminal;
and `--stop` ends it, with the endpoint file naming its pid so a wedged one can
still be found. Version skew is refused at `hello` by name in both directions,
because a daemon left running across an upgrade is the expected case.

Not yet moved: the four ingest channels still start in the window. ADR-0026's
fixed-port bind makes them a singleton and the daemon is their natural owner, and
until they move, a detached period records nothing — so "shut the lid for an hour
and watch it play back on the map" is still ahead. `Mode::Work` constructs
`Ingest` at a single call site so that stays a one-edit change.

---

## ADR-0099 — The drawn anchor is the field's mode; the centre of mass stays a mean

**Context.** PRD §6 opens on a single sentence about where a thread's mark may
go:

> A main agent has no meaningful point location — it delegates rather than
> edits. Computing a centroid of its workers is actively wrong: an orchestrator
> with workers in `src/auth` and `tests/` gets a centroid in the empty gap
> between them, which is the one place nothing is happening.

`polis-world/src/territory.rs` has quoted that paragraph at the top of the file
since it was written. `Territory::refresh_centre_of_mass` is `Σ(c·w)/Σw` — the
centroid the paragraph rejects, by name — and **every** drawn mark read it: the
anchor ring and the tether fan (`polis_app::mapview`), the headless
`polis_render::frame::thread_anchor` through `place::thread_position`, the
attention layer's `mark_position`, the on-map name of a waiting thread, and the
follow camera's cut. The operator reported both halves of the consequence: a
thread's ring drawn in empty space away from its own cloud, tethers fanning out
of that empty point, and two different sessions landing on nearly the same
anchor.

The second half is the sharper one and it is arithmetic, not tuning. A mean
keeps only a field's first moment, so it discards exactly the information that
distinguishes two shapes. Two sessions touching the same repository from
different directions therefore *converge* on the same mark — a session working
`src/services` and `src/hooks` and a session working `src/components` and
`src/pages` can share a centre exactly, and
`two_threads_in_different_places_do_not_share_an_anchor` builds that case in four
lines.

Three consumers had already routed around the mean privately, which is the sign
that the field was wrong rather than the callers: `place::agent_position`'s doc
demoted it to rung 3 on a measurement (*"the centre of mass sat 227 city units
from the median of the files the session touched, which put every rung-3 mark
off the side of a camera framed on the work"*), `polis_app::app`'s follow camera
put the trail head ahead of it, and `polis_app::clouds` computes a heaviest-
cluster centroid of its own for the bridge band. None of them said why in a
place the next person would look.

**Decision.** Split the two meanings and give each its own name.

`Territory::centre_of_mass` **stays exactly as it is**, and stays a mean, because
two consumers genuinely want a first moment: `contention`'s
`CloudSummary::centre` feeds a 4σ bounding rejection, which is a statement about
spread, and `place::thread_position` keeps it as the last rung so a territory
assembled by hand — kernels never pushed through `observe` — still answers.
Its doc-comment's claim that *"the drift vector is measured against this"* was
false and is deleted: `drift_state` and the drift trace both call the private
`weighted_centre` over time-windowed *subsets* and never read the field.

The **drawn** anchor becomes `Territory::anchor` — the kernel centre at which
the density field is highest. Not a centroid, not a medoid, not the claim's
district:

* the claim's district centre is **absent in 42 % of samples** (18 of 43), which
  is precisely the orchestrator case §6.4 exists for, and reinstating it would
  put the mark back in the middle of the map for a root-scoped thread;
* the weighted medoid is the same O(k²) and measurably worse (0.964 density at
  p10 and 10.0 units of separation, against the mode's 1.000 and 12.3);
* *"the most-edited file"* has no data inside `Territory` at all — `Evidence`
  folds to the parent directory and `Kernel` carries no path.

Measured across the same 43 field-bearing samples: density at the drawn point
rises from **0.0103 to 1.000 of the field's peak at p10**, and the separation
between the marks of a disjoint-lobe session pair rises from **5.8 to 12.3 units
on a 352-unit city**, worst measured pair `e41f2794` against `dfa8cd66`, **2.3 →
16.4**.

**The curve is shared, not copied.** The anchor is only the right point if it is
the argmax of *the field that is actually drawn*, so `polis_layout::quartic` now
holds the one definition of `(1 - r²)²` and both `polis_render::live::kernel` and
`Territory::density_at` call it. `polis-world` cannot depend on `polis-render`,
so the alternative was a second definition — the failure `polis-layout`'s own
module doc already names about `Vec2::length`, and here it would mean the ring
being the peak of one function and the contours the bands of another. A Gaussian
was rejected for the reason PRD §7.4 gives (`exp` is a transcendental and the
rasteriser has none); the quartic is also about four times cheaper and has
compact support.

**No clock, and that is a determinism decision.** The obvious shape for an O(k²)
recompute is a wall-clock throttle plus hysteresis. It would break window/
headless parity (ADR-0029): `decay` runs from `World::tick`, `ReplayDriver::
advance` ticks once per advance and `run_to_end` ticks once for a whole
schedule, so a throttled state machine would settle on different anchors in the
window and in a recorded GIF of the same recording. It is also unnecessary,
because **a uniform rescale cannot move an argmax** — the decay factor and
`rest`'s lift are both uniform, so the memo is carried through them
arithmetically. The scan runs only when the kernel *list* changes: a push in
`observe`, the window eviction beside it, and `decay`'s `retain` on the ticks it
actually drops something. Ties are broken on `(density, x, y)` with
`f32::total_cmp`, never `partial_cmp`, because equal densities are the common
case here and not the corner one.

**`ANCHOR_MARGIN = 1.25` is PRD §6.3's sentence, for the anchor.** §6.3 says
*"the territory's centre of mass may only shift districts after N=8 consecutive
weighted observations […] one read elsewhere moves nothing"*, and
`DRIFT_CONFIRMATIONS` implements it for the *claim* only. Nothing implemented it
for the drawn point, because a mean slides and never jumps; a mode does. The
mode's median frame-to-frame jump is 0.26 bandwidths against the mean's 1.28 —
much calmer — but its p90 is 14.68, and that p90 is the near-tie teleport and
nothing else. The margin converts near-tie flicker into one decisive move, and
buys a bound: the drawn point's density is never below `1 / ANCHOR_MARGIN` of
`field_peak`, so hysteresis cannot park the ring somewhere cold.

**Consequences.** One ladder, walked from one place. `polis_app::mapview` now
calls `polis_world::place::thread_position(thread, &snapshot.layout)` for the
ring and the tether fan rather than reading `centre_of_mass` itself — which also
closes a parity bug that predates this change, since the headless renderer has
always walked that ladder and the window had a shorter one, so a thread with a
trail and no kernels got a ring in a recorded frame and none in the window. The
attention layer's `mark_position` and the waiting thread's on-map name substitute
only the first rung (`anchor()` falling back to `centre_of_mass`) and keep their
`placement()` rungs, which exist for the documented reason that a thread with 483
calls once had nowhere to put its pin.

**The corpus is one operator, two repositories, 43 field-bearing samples**, with
`Territory::lobes` as its own ground truth — a lobe set is what says two sessions
are working in different places, and it is derived from the same evidence the
anchor is. `ANCHOR_MARGIN` is **asserted, not swept**: 1.25 was read off the jump
distribution above rather than chosen by sweeping the constant, and the harness
that took those numbers is not in the tree, so they cannot currently be re-taken.
`CLOUD_CAP`'s caveat applies here with more force, because that one at least had
a fleet behind it.

No PRD amendment: nothing in the PRD specifies the anchor mark, and this moves
the code *toward* §6's opening paragraph rather than away from it.

---

## ADR-0100 — A thread proves it is alive by working, not by working *somewhere*; and what a resting cloud has to clear is a field value

**Context.** The operator's report was *"i dont see any clouds"*, on a live map
whose trails, glyphs and agent marks were all drawing correctly. Two independent
defects produced it, and either one alone is enough.

**The first is a unit error inside `polis_world::territory`.** PRD §10.4's
dormancy gate asks how long a thread has been quiet, and `Territory::quiet_for`
answered it from the newest entry in `Territory::evidence`. But `observe` drops
every observation whose claim path is the repository root, because the root is
the absorbing element of §6.2's lowest common ancestor and one live root-scoped
entry pins `depth(A)` at 0 for as long as it lives — see PRD §6.1's amended
`Bash` cwd row. A shell call's only path signal is its `cwd`, and a `cwd` at the
checkout root is the common case rather than the corner one: **11 605 of 20 246
observations, 57.3 %**, on the corpus this was measured against. So an agent in
the middle of a build-and-test stretch produced a call a second, every one of
which was thrown away, and after `DORMANT_AFTER` its territory read dormant while
it was demonstrably working. `select_clouds` then dropped it, `rest` stopped
holding its field up, and `decay`'s collapse deleted its claim and its lobes as
soon as the *non-shell* evidence decayed out from under them.

**Decision.** `Territory` carries `last_observation`, stamped by `observe`
**before** the root drop, and `quiet_for` reads it — falling back to the evidence
maximum, which is what keeps every hand-built cloud fixture in the workspace
working. `World::observe` counts the drop in
`Health::root_scoped_observations`. And `decay`'s collapse now waits for the same
dormancy test rather than firing the moment the evidence list empties, so a
converged claim outlives a shell-only stretch. The two halves are one object:
under the old `quiet_for` the gated collapse could never fire at all, because an
empty evidence list makes `quiet_for` return `None`.

**The mechanism matters, and the plan for this change had it wrong.** The
investigation attributed the disappearing cloud to the evidence collapse at
roughly 600 s (a weight-1 entry survives about 6.6 half-lives). It is not: `rest`
returns before doing anything past `DORMANT_AFTER`, `select_clouds` drops the
territory on the same test, and `DORMANT_AFTER` is **480 s**. The cloud dies at
eight minutes through the dormancy gate. The `last_observation` change is the
whole fix; re-gating the collapse is second-order, and is here because a claim
that outlives its own cloud is the rail and the map disagreeing again.

**The second is a unit error across the crate boundary.** `RESTING_WEIGHT = 0.9`
exists so that a converged territory that goes quiet keeps a drawable field
instead of decaying to nothing — the operator's *"clouds should be there
immediately"*. Its doc justified the value against
`polis_render::live::CLOUD_ISO`, whose outermost band is `0.55`. But `CLOUD_ISO`
thresholds the **density field** (ADR-0020: one full-weight kernel reads 1.0 at
its own centre) and `rest` normalised the **mass**, the sum of the weights. Those
are the same number only for a territory whose kernels sit on one point, and PRD
§6.4's multi-lobed territory is spread by construction. Measured over 472
selected clouds: **33 of them, 7.0 %, were computed, ranked, handed to the
rasteriser and never painted** — mean mass 1.136, comfortably over the floor;
mean peak 0.429, under the fringe.

**Decision.** `rest` normalises `Territory::field_peak`, which is the exact
quantity `CLOUD_ISO` is thresholded against, evaluated with the same
`polis_layout::quartic` the rasteriser splats (ADR-0099). The value 0.9 stands,
now as a peak, and the **1.64x headroom over the 0.55 fringe is the reason it
works rather than an accident**: three separate losses sit between the world's
peak and the drawn one, all on the window's side of the house —
`polis_app::clouds` floors each drawn radius at `.max(2.0)` where
`polis_render::frame` does not, it adds a chain of `BRIDGE_WEIGHT` kernels
between lobes, and `CloudField::sample` reads a lattice at cell centres and
under-reads a sharp peak by the sub-cell offset. At 0.55 exactly, any one of them
would put the cloud back under the fringe.

**No per-tick cost.** `rest` runs from `decay`, which runs for every thread on
every `World::tick`, and a peak is an O(k²) scan bounded by `OBSERVATION_WINDOW`
squared. It is not paid: ADR-0099 landed `field_peak` as a value memoised beside
the anchor, and both rescales in play — decay's `factor` and `rest`'s own `lift`
— are uniform, so the memo is carried through them by one multiply and the scan
happens only when the kernel *list* changes.

**Instrumentation, because both defects were silent.** The window called
`territory::visible_clouds`, a wrapper that returns the chosen territories and
drops `unplaced`, `dormant` and `capped` on the floor, so the status bar could
only ever print `0 clouds (0 kernels)` — the symptom stated twice and the cause
not at all. It now calls `select_clouds` directly, keeps the whole
`CloudSelection`, and builds the `CloudCensus` the headless renderer has carried
since it was written; the bar reads `0 CLOUDS · 4 UNCONVERGED`. `widest_px` is
resolved in **output pixels** at the call site that knows the camera, because
`CloudCensus::sub_pixel`'s *"ZOOM IN"* hint is a statement about the operator's
scroll wheel while the cloud layer's own frame is a fixed texel grid. The rail's
`unplaced` hover shows the undecayed total beside the live evidence count, so a
thread with 417 tool calls stops reading *"0 observations"*.

**Consequences.**

* A thread running nothing but root-scoped shell commands is **not** dormant, and
  its cloud stays over whatever it was last working on for as long as it keeps
  running them. That is the honest reading — the field is stale about *where*,
  and §6.3's decay is what already says so by letting it fade — where "dormant"
  was a false claim that the agent had stopped.
* `DORMANT_AFTER` becomes load-bearing for the **claim's** lifetime and not only
  for the cloud's. Its doc-comment's sweep table was taken at 180 s, before
  `ab844b1` moved the constant to eight minutes and left the justification
  untouched; that table is corrected in place rather than deleted, and one of its
  readings — *"at 600 s the gate has nothing left to catch, since the territory
  has dissipated on its own"* — is now false, because `rest` means nothing
  dissipates on its own any more.
* Every resting territory is **brighter and wider** than before, since a peak
  crosses the floor sooner than the mass it is a fraction of. `CLOUD_CAP = 5` was
  chosen at the 1.5-mean-crowd crossover on the old brightness, so both real
  harnesses were re-run on each side of the change, on the operator's own
  sessions. `cloud_cap_policy`'s contested-fraction sweep is **identical to the
  digit** at every cap — at its measurement instant every session is on its own
  busiest minute, so nothing is resting and `rest` never fires. The cap is
  therefore not re-baselined. What *did* move is that harness's dormancy sweep,
  and only in the short windows: 13 → 15 territories survive 30 s, 16 → 19
  survive 90 s, 22 → 24 survive 180 s, and 24 → 24 at 600 s and above. That is
  `last_observation` doing exactly and only its job. `cloud_measure` moved the
  same way and no further: 53 → 54 of 64 frames put a cloud on the map, dormant
  0.03 → 0.02 per frame, kernels 34.6 → 35.8, worst ink share 51 % → 49 %, with
  `disturbed_px = 0` and `under_cloud_median_shift = 0.000` unchanged. No
  assertion in either harness was touched.
* **A separate finding, which this change did not cause and does not fix.** The
  fresh `cloud_cap_policy` sweep does not reproduce the cliff `CLOUD_CAP`'s doc
  argues from: contested ink goes 27.0 % → 27.9 % from five clouds to six, not
  37.7 % → 58.3 %, and the mean-crowd 1.5 crossing has moved from five to six.
  The fleet changed underneath the table — PRD §6.4's lobes (`fb414aa`) place 24
  of these forty sessions where 11 were placed before. Read strictly the sweep
  now supports a cap of six. The cap is held at five and the disagreement is
  written into its doc-comment, because what the operator sees is a decision and
  not a consequence of this fix.
* Both measurement harnesses now say in their headers that they measure the
  shipped path **at its best moment**: `cloud_measure` samples through
  `pacing::plan`, which weights frames by event mass, and `cloud_cap_policy`
  aligns every session on its own peak. Event-weighted sampling gives 94 % cloud
  coverage where wall-clock sampling gives 62 %, and that gap is why both were
  green while the map was empty. The wall-clock-sampled coverage harness that
  would catch it is **not written**, and the before/after numbers above should be
  read with that limit in mind: they are measured where the clouds were already
  working.

**Deferred: shell evidence at depth 1.** `shell::MIN_EVIDENCE_DEPTH = 2` discards
every depth-1 token, and in a repository whose directories are all one level deep
— this one — the shell channel therefore contributes no scope whatever, which
`shell.rs`'s own doc already anticipates. Admitting depth-1 tokens as lobe-only
evidence would recover it, and would need a new field on `Evidence`, three
separate exclusions in `convergence()` and a second denominator in `lobes_of`.
The recovery was never measured and the change risks resurrecting the
asymmetric-noise failure that gate exists for. Not done here.

**No amendment to PRD §6.2, §6.3 or §10.4.** All three are obeyed; the code was
not. PRD §6.1 **is** amended in place, because its `Bash` cwd row describes a
contribution the implementation deliberately does not make.

---

## ADR-0101 — The identity hue is assigned by the world, not derived by the renderer

**Context.** The operator ran two live sessions and could not tell them apart:
*"the same color is super bad, can you make sure the colors have to be
different?"*. `polis_render::live::thread_slot` was FNV-1a over the thread id
modulo twelve, computed independently at five draw sites, and its own
doc-comment accepted the collisions on purpose — *"Slots are handed out by
hashing, so two threads can land on one hue — with nine threads on screen and
twelve slots that is about three of the thirty-six pairs. That is the price …
and it is paid on purpose."* The two ids `"a"` and `"polis"` both hash to slot 4,
and `ThreadId::of_session` is the identity function on the session id, so that is
a collision a real `World` can hold.

Two alternatives were considered and rejected before this one:

* **More hues.** `THREAD_HUES`'s measured ΔE00 table already prices it. Twenty
  slots would take the expected number of distinct colours among nine threads
  from 6.6 to 7.4 while cutting the worst pair from 9.80 to 5.4 — into the band
  the cloud fringe already occupies at 5.46, which is the level that needs a
  second channel to be read at all. And it only lowers the collision *rate*; it
  does not satisfy "guaranteed distinct".
* **Probing over the live thread list at draw time.** Neither stable nor
  deterministic. Thread A (preference 3, first) and B (preference 3, probed to
  4); A retires; a recomputation from the live set leaves B alone with
  preference 3 and moves it 4 → 3 — an unrelated thread *ending* repainted B,
  which is exactly what the hash was chosen to prevent.

**Decision.** The hue slot is assigned **once, by the world, at thread
creation**, and stored on `polis_world::Thread::tint`. Three parts:

1. `polis_events::IDENTITY_SLOTS = 12` and `ThreadId::hue_preference()` move the
   FNV hash down into the event model, beside `LogicalPath::layout_seed` and
   under the same ADR-0029 rule. `polis-world` depends on `polis-events` and not
   on `polis-render`, which is why the constant — the length of a colour table
   two crates away — is declared there. A `const _: () = assert!(…)` beside
   `THREAD_HUES` is what stops the two drifting.
2. A private `HueRing` on `World` holds one thread per slot. `World::thread_entry`
   claims a slot the first time a session is seen: the thread's own preference if
   it is free, else a fixed index probe `(pref + k) % 12`, else nothing.
3. `live::thread_slot` and `palette::thread_slot` are **deleted**, not shimmed,
   and all five draw sites become field reads. A surviving function of that name
   is an invitation to re-derive, and the point of the change is that there is
   one answer.

**A slot is never released.** A thread leaving the world does not give its colour
back, and the ring remembers *which id* took each slot, so a session retired for
silence and heard from again gets the colour it had. This is the load-bearing
half of the decision and it is a determinism argument, not a product one.
Retirement runs from `World::tick`, and tick cadence differs between the two
renderers on the same recording: `ReplayDriver::advance` ticks once per call and
the window calls it many times, so `retire_threads` fires repeatedly mid-run;
`run_to_end` applies the whole schedule and ticks exactly once. Two things would
otherwise become functions of the tick cadence — whether a slot is free when the
next thread is created, and how many times a given thread is created at all — and
the window and a recorded GIF would paint one thread two colours. That is the
failure `polis_app::palette` named in prose: *"a colour that meant one thread in
the window and another in a recorded GIF would be worse than no colour at all."*
What is left depends only on the ordered set of distinct thread ids, which is the
event order, which both drivers share.

**The guarantee, stated precisely.** *The first twelve threads of a world are
mutually distinct; past that, colour degrades to the bare preference and the
rail's name carries identity (PRD §11.4).*

**Consequences.**

* A world degrades after twelve threads have **ever** been seen in it, not twelve
  concurrently. Past twelve the thirteenth thread does not merely risk a
  collision, it is certain to have one, because every slot is taken.
* **THE GUARANTEE IS ALREADY ONE SLOT SHORT OF THIS OPERATOR'S OWN FLEET, AND
  THAT IS NOT HIDDEN.** Measured over their `~/.claude/projects` — 193
  main-session transcripts, subagent sidecars excluded because a subagent carries
  its parent's session id (ADR-0030): peak threads live at once, counting a
  session live until its last record plus `THREAD_RETIRE_AFTER`, is **13**. Per
  day they start a median of 15 distinct sessions, and 8 of 15 days exceed
  twelve. A Polis left running reaches its twelfth distinct session in a median
  of 17 hours (p10 2.4). So this removes the collisions that were reported —
  three shared pairs among nine live threads — and does not remove them all; at
  the busiest moment one thread still shares. That is counted in
  `Health::identity_hues_exhausted` and shown in the status bar as
  `shared hues N`, so the residue is a reading rather than a re-report. The
  operator's remedy is a restart, which is a `World::reset`, which clears the
  ring. The measurement came from a scratchpad script, not from a test in the
  tree, and cannot currently be re-taken.
* If the fleet keeps growing, the fix is **not** a thirteenth hue — see the
  rejected alternative above — it is a second channel on the rail row.
* Widening the ring is **not** the answer to exhaustion — see the rejected
  alternative above. `THREAD_HUES`'s doc now carries that argument in the
  reversed direction it now runs in, and
  `thread_hues_are_far_enough_apart_at_the_luminance_they_are_drawn_at` is what
  fires if anyone widens it anyway.
* The ring's memory is bounded at twelve ids by construction: once every slot is
  taken nothing more is recorded.
* ADR-0029 is amended in place. The hue *preference* is still per-id and still
  pinned by literals; the hue *drawn* is now per-event-sequence. That is a real
  weakening and it is written into that ADR rather than left for a reader to
  discover.
* A `Thread` built outside a `World` — every fixture in the workspace — keeps its
  bare preference and is **not** exclusive. Deliberate: exclusivity is a property
  of the set of live threads and a fixture has no set. No existing fixture
  changes colour, and no golden file moves.

**Known limit, untouched by this change: the GIF encoder.**
`polis_render::gif` median-cuts every frame to a 256-entry palette, so two hues
made exclusive here can still be **merged by the encoder** in recorded output.
That is an independent second cause with its own fix, and post-quantisation ΔE00
across the twelve hues was deliberately not measured here. "Guaranteed distinct"
is a claim about the world and about what the window and the headless rasteriser
paint; it is not yet a claim about a GIF.

**Also not fixed: the places the hue is absent rather than colliding.**
`polis-app/src/status.rs` and `polis-app/src/treeview.rs` carry no per-thread hue
at all. If the operator's complaint is "I cannot tell two threads apart", those
are a larger gap than the collision this ADR closes, and a separate defect.

**No PRD amendment.** §11.4 (*"Colour alone is never the sole channel for any
state"*) is unaffected — this makes colour more reliable, not sole — and §10.3
and §10.4 constrain brightness and cloud count, not hue count.
