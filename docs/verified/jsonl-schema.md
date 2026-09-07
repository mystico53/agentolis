# Verified: Claude Code JSONL transcript schema (Channel D)

Companion to `hooks-schema.md`. Source of truth is the files on this machine and
the shipped `claude.exe`, not documentation — none exists.

**Corpus.** `C:\Users\konka\.claude\projects`, scanned 2026-09-01:
**864 `.jsonl` files, 153 613 records, 12 project directories, 2 CLI versions
(2.1.229 and 2.1.248).** Zero lines failed to parse as JSON. Every quantity
below is a count over that corpus, not an estimate.

**Verification of the model.** The serde model in §12 parses all 153 613 records
with `bad_json = 0`, `bad_shape = 0`, `unknown_type = 0`, and the fields Polis
depends on are demonstrably populated (`toolUseResult` 23 316, `promptId`
39 664, `requestId` 66 366, subagent `agentId` 37 615). That corpus test is the
strongest claim in this document; treat everything else as commentary on it.

Legend used throughout:
- **STABLE** — present on ≥ 99% of records of that type. Model as non-`Option`
  only if you also accept a hard failure on drift; otherwise `Option<T>` with a
  `.expect_or_log`.
- **INCIDENTAL** — present sometimes. Always `Option<T>`.
- **ABSENT-MEANS-FALSE** — a `bool` that is simply omitted when false.

---

## 1. Where the files live

### 1.1 The `<munged-cwd>` rule — PRD §4.4 is right, and here is the exact algorithm

Extracted from `claude.exe` (a Bun single-file binary, `grep -a`), then
**confirmed end-to-end against the live CLI**:

```js
function hash(t){ let e=0; for(let r=0;r<t.length;r++) e=(e<<5)-e+t.charCodeAt(r)|0; return e }
const LIMIT = 200;
function projectKey(t){
  let e = t.replace(/[^a-zA-Z0-9]/g, "-");
  if (e.length <= LIMIT) return e;
  return `${e.slice(0, LIMIT)}-${Math.abs(hash(t)).toString(36)}`;
}
```

Three details the PRD does not state and an implementer will get wrong:

1. **The hash is taken over the ORIGINAL `cwd`, not the munged string.**
2. `.length` and `.slice` are **UTF-16 code units**, and `charCodeAt` yields
   UTF-16 code units — so a non-ASCII path must be hashed over `encode_utf16()`,
   not `chars()`. (After munging, every non-ASCII char has already become `-`,
   so only the *hash input* is affected.)
3. `Math.abs` on `i32::MIN` yields `2147483648` in JS, which does not fit in
   `i32`. Rust must widen to `i64` before taking the absolute value or it panics.

**Empirical confirmation.** A directory whose absolute path is 218 characters
was created and `claude --print` was run inside it. Predicted and actual
directory names matched byte-for-byte, including the base-36 suffix:

```
cwd  (218 chars) C:\Users\konka\AppData\Local\Temp\claude\C--coding-agentolis\6f51089f-…\scratchpad\aaaaaaaaaa\…\jjjjjjjjjj
dir  (207 chars) C--Users-konka-AppData-Local-Temp-claude-C--coding-agentolis-6f51089f-…-scratchpad-aaaaaaaaaa-…-hhhhhhhhhh-iii-w6jyaf
                 └───────────────────────── 200 chars ─────────────────────────┘ └── "-" + base36 ──┘
```

All 12 existing project directories also round-trip exactly under this rule; the
character set across every directory name is `[A-Za-z0-9-]` with no exceptions.

**Caveat — the key can be overridden.** The CLI computes the project key as
`override() ?? projectKey(cwd)`. The override path exists in the binary (guarded
by a filename-safety regex plus a Windows reserved-name check for
`con|prn|aux|nul|com[0-9]|lpt[0-9]`). It was not exercised in this corpus.
Polis must therefore **discover directories, not only compute them**: compute
`projectKey(cwd)` as the primary lookup, but also watch `~/.claude/projects/`
itself for new directories, and read `cwd` out of the records to learn the real
mapping. Never assume a session's directory can be derived from `cwd` alone.

### 1.2 On-disk layout — PRD §4.4 understates the nesting

Normalized inventory of every path under `~/.claude/projects`:

```
<munged-cwd>/<session-id>.jsonl                                              168   main transcript
<munged-cwd>/<session-id>/subagents/agent-<agentId>.jsonl                    140   directly-spawned subagent
<munged-cwd>/<session-id>/subagents/agent-<agentId>.meta.json                140   its parent link
<munged-cwd>/<session-id>/subagents/workflows/wf_<runId>/agent-<agentId>.jsonl   503   workflow subagent
<munged-cwd>/<session-id>/subagents/workflows/wf_<runId>/agent-<agentId>.meta.json 503
<munged-cwd>/<session-id>/subagents/workflows/wf_<runId>/journal.jsonl        40   per-run agent journal
<munged-cwd>/<session-id>/workflows/wf_<runId>.json                           40   workflow definition
<munged-cwd>/<session-id>/tool-results/<9-char-id>.txt                       …    overflowed tool output
<munged-cwd>/memory/*.md                                                     …    not transcripts
<munged-cwd>/bridge-pointer.json, .session-aliases                           …    not transcripts
```

The subagent path in the binary is
`projects/<key>/<sessionId>/subagents/...<agentRelPath>/agent-<agentId>.jsonl`,
where `agentRelPath` is `[]` for a directly spawned agent and
`["workflows", "wf_<runId>"]` for a workflow one. It is a **variable-depth
path** — do not hardcode two levels; glob for `agent-*.jsonl` under `subagents/`.

**A tailer must watch four things**, not one:
`<session-id>.jsonl`, the `<session-id>/subagents/**` tree (files appear at
runtime as agents spawn), the `.meta.json` beside each new agent file, and the
`tool-results/` directory for outputs too large to inline.

### 1.3 🚨 Windows: a transcript path can exceed `MAX_PATH`

The 200-char cap applies to the *directory name*, not the full path. The
directory produced by the §1.1 experiment is 207 characters, and the transcript
inside it is at

```
C:\Users\konka\.claude\projects\<207-char dir>\0aa2c7ad-….jsonl     282 characters
```

with `HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\LongPathsEnabled = 0` on
this machine. The CLI (Bun) writes it happily; it uses extended-length paths
internally.

Measured consequences:

| accessor | result |
|---|---|
| Rust `std::fs::read` / `File::open` / `metadata` | **works** (std converts to `\\?\` verbatim paths internally) — verified by test |
| `fs::read_dir` over the parent | **works** |
| Python `io.open()` | **fails**, `FileNotFoundError` — verified |
| `PowerShell Get-ChildItem -LiteralPath` | works |

So the Rust ingest path is safe as long as it stays in `std::path::Path`. What
is *not* safe: any helper that round-trips a path through a `String` and
re-joins it, any shell-out to a tool that lacks long-path support, and any
auxiliary Python tooling. Verify `notify`'s behaviour on such a directory before
relying on Channel C for a deep-cwd project, and never assume a path that
`read_dir` returned can be handed to an external process.

---

## 2. Top-level `type` histogram

| `type` | n | Threaded? | What it is |
|---|---:|---|---|
| `assistant` | 66 098 | yes | one model response (may be a single thinking block) |
| `user` | 39 686 | yes | a human turn **or** a tool result |
| `attachment` | 18 985 | yes | injected context (reminders, tool listings, file snippets) |
| `mode` | 4 458 | no | session sidecar |
| `last-prompt` | 4 365 | no | session sidecar |
| `bridge-session` | 4 327 | no | session sidecar |
| `permission-mode` | 2 886 | no | session sidecar |
| `ai-title` | 2 835 | no | session sidecar |
| `custom-title` | 2 521 | no | session sidecar |
| `agent-name` | 1 826 | no | session sidecar |
| `queue-operation` | 1 551 | no | session sidecar |
| `file-history-delta` | 906 | no | edit-undo bookkeeping |
| `system` | 904 | yes | turn duration, compaction, local commands |
| `file-history-snapshot` | 609 | no | edit-undo bookkeeping |
| `started` | 509 | no | **`journal.jsonl` only** |
| `result` | 466 | no | **`journal.jsonl` only** |
| `atis-latch` | 110 | no | session sidecar |
| `frame-link` | 16 | no | session sidecar |
| `cost-state` | 3 | no | session sidecar |

**The single most important structural fact:** only `user`, `assistant`,
`attachment` and `system` carry `uuid`/`parentUuid` and participate in the tree.
The other 15 types are **flat session sidecars**: `{type, sessionId, <1-3 fields>}`
with no uuid, no timestamp on most, and no position in the conversation. They
are appended interleaved with real records. A parser that assumes every line has
a `uuid` will break on 25% of lines.

---

## 3. The envelope (threaded records only)

Present on `user` / `assistant` / `attachment` / `system`.

| field | presence | type | notes |
|---|---|---|---|
| `uuid` | STABLE 100% | string (uuid v4) | **0 duplicates in 124 262 records**. Unique per file. |
| `parentUuid` | STABLE 100% | string \| **null** | Explicitly `null` on tree roots. Key must exist; value may be null. |
| `isSidechain` | STABLE 100% | bool | `true` ⟺ the record lives in a `subagents/` file. See §8. |
| `timestamp` | STABLE 100% | string | See §10. |
| `sessionId` | STABLE 100% | string (uuid) | Equals the *file's* session id — **including inside subagent files**. |
| `cwd` | STABLE 100% | string | Absolute. On Windows, backslashes, 125 050/125 050. |
| `gitBranch` | STABLE 100% | string | Can be `"HEAD"` (detached) — 4 463 records. Not always a branch name. |
| `version` | STABLE 100% | string | CLI version, e.g. `"2.1.229"`. Your drift signal. |
| `userType` | STABLE 100% | string | `"external"` in 125 673/125 673. |
| `entrypoint` | STABLE 100% | string | `"cli"` (125 553) \| `"sdk-cli"` (120). |
| `agentId` | see §8 | string | Present ⟺ subagent file. 17-char `a`-prefixed hex. |
| `session_id` | INCIDENTAL ~40% | string (uuid) | **Not** a duplicate of `sessionId` — see below. |
| `slug` | INCIDENTAL ~38% | string | Human nickname, e.g. `graceful-leaping-steele`. 26 distinct. |

### 3.1 `session_id` vs `sessionId` — a genuine trap

Both keys can appear on the same record. They are **equal on 22 180 records and
different on 38 153**. Where they differ, `session_id` (snake) is the **ancestor
session** the current one was resumed or forked from. Example: file
`68160373-….jsonl` has `sessionId = 68160373-…` (matches its own filename) but
`session_id = 1ddc1a10-…`, which is a different, earlier session file in the same
project directory.

8 of 136 files carry more than one distinct `session_id`, i.e. multiple resume
lineages inside a single file.

For Polis this is the field that stitches a `--continue`d session back to its
ancestor. Do **not** deserialize it into the same struct field as `sessionId`;
`serde(rename_all = "camelCase")` will happily map `session_id` → nothing and
`sessionId` → `session_id`, which is exactly backwards. Model both explicitly.

---

## 4. Per-type field unions

Percentages are of records of that type. Anything not listed was never seen.

### 4.1 `assistant` (n = 66 098)

STABLE 100%: `cwd` `entrypoint` `gitBranch` `isSidechain` `message` `parentUuid`
`sessionId` `timestamp` `type` `userType` `uuid` `version`; `requestId` 66 090
(100.0%); `effort` 66 036 (99.9%, `"xhigh"` \| `"high"` \| `"max"`).

INCIDENTAL:

| field | n | % | notes |
|---|---:|---:|---|
| `agentId` | 37 379 | 56.6 | subagent files only |
| `attributionAgent` | 37 369 | 56.5 | **agent *type*, not an id** — see §8.5 |
| `session_id` | 28 677 | 43.4 | §3.1 |
| `slug` | 24 859 | 37.6 | |
| `attributionSkill` | 4 053 | 6.1 | skill that produced the turn |
| `attributionMcpServer` / `attributionMcpTool` | 3 764 | 5.7 | |
| `attributionPlugin` | 704 | 1.1 | |
| `isApiErrorMessage` | 27 | 0.0 | ABSENT-MEANS-FALSE |
| `error` | 21 | 0.0 | string |
| `apiErrorStatus` | 20 | 0.0 | int (HTTP status) |
| `isAbortedMidStream` | 4 | 0.0 | ABSENT-MEANS-FALSE |

`message` (assistant) is 100% `{model, id, type, role, content, stop_reason,
stop_sequence, stop_details, usage, diagnostics}`. `stop_details` is `null` in
65 262/65 262. `diagnostics` is `null` except 371 records where it is
`{cache_miss_reason}`. `stop_reason` ∈ `tool_use` (38 732) \| `end_turn` (958) \|
`stop_sequence` (26) \| `null`. `message.model` ∈ `claude-opus-5`,
`claude-fable-5`, `claude-sonnet-5`, `claude-haiku-4-5-20251001`, `<synthetic>`.

`message.usage` always has `input_tokens`, `output_tokens`,
`cache_creation_input_tokens`, `cache_read_input_tokens`, `service_tier`,
`cache_creation {ephemeral_1h_input_tokens, ephemeral_5m_input_tokens}`,
`inference_geo`; and on ~60% also `output_tokens_details`, `server_tool_use`,
`iterations` (array of per-request usage), `speed`.

### 4.2 `user` (n = 39 686)

STABLE 100%: the envelope + `message`. Note `parentUuid` is `null` on 811.

| field | n | % | notes |
|---|---:|---:|---|
| `promptId` | 39 502 | 99.5 | **Groups a whole turn.** See §8.4. |
| `sourceToolAssistantUUID` | 37 912 | 95.5 | Points at the assistant record that issued the tool_use. **Equals `parentUuid` in 37 912/37 912** — free integrity check, no new information. |
| `toolUseResult` | 23 293 | 58.7 | object 20 479 / array 1 744 / **string 1 070**. Shape is per-tool. See §7. |
| `agentId` | 23 191 | 58.4 | subagent files only |
| `session_id` | 15 551 | 39.2 | §3.1 |
| `slug` | 15 032 | 37.9 | |
| `origin` | 655 | 1.7 | `{kind}` ∈ `human` (485) \| `task-notification` (159) \| `coordinator` (3) |
| `permissionMode` / `promptSource` | 644 | 1.6 | on real human turns |
| `toolEndsTurn` | 389 | 1.0 | |
| `isMeta` | 273 | 0.7 | injected, not typed by the human |
| `classifierMetaLines` | 194 | 0.5 | |
| `toolDenialKind` | 69 | 0.2 | `user-rejected` 53 \| `permission-rule` 11 \| `automode-blocked` 4 \| `automode-unavailable` 1 |
| `mcpMeta` | 56 | 0.1 | `{structuredContent, _meta}` |
| `interruptedMessageId` | 42 | 0.1 | |
| `sourceToolUseID` | 35 | 0.1 | |
| `imagePasteIds` | 24 | 0.1 | array |
| `userFeedback` | 12 | 0.0 | |
| `turnCompanion` `queuePriority` `isCompactSummary` `isVisibleInTranscriptOnly` `queueSkipAttachments` | ≤ 8 | 0.0 | |

`message` (user) is always `{role, content}` and nothing else.

`promptSource` ∈ `typed` (474) \| `system` (146) \| `queued` (10) \| `sdk` (8) \|
`suggestion_accepted` (6). `permissionMode` ∈ `auto` (3 172) \| `plan` (352) \|
`default` (3) \| `acceptEdits` (3).

### 4.3 `attachment` (n = 18 985)

Envelope + `attachment: {type, …}`. `agentId` on 1 342 (7.1%).
`attachment.type` (25 values seen — treat as open):

| type | n | payload keys |
|---|---:|---|
| `total_tokens_reminder` | 14 027 | `text` |
| `task_reminder` | 1 782 | `content`, `itemCount` |
| `skill_listing` | 835 | `content`, `skillCount`, `isInitial`, `names` |
| `deferred_tools_delta` | 762 | `addedNames`, `addedLines`, `removedNames`, `readdedNames`, +`pendingMcpServers`, `wireHiddenNames`, `failedMcpServers` |
| `queued_command` | 290 | `prompt`, `commandMode`, `timestamp`, +`origin`, `imagePasteIds`, `source_uuid` |
| **`edited_text_file`** | 287 | **`filename`**, `snippet` — a human edit outside the agent |
| `agent_listing_delta` | 145 | `addedTypes`, `addedLines`, `removedTypes`, `isInitial`, `showConcurrencyNote` |
| `mcp_instructions_delta` | 145 | `addedNames`, `addedBlocks`, `removedNames` |
| `auto_mode` | 143 | `autoModeConsentFlow`, `bashFirst`, `steerOnly`, `bypass` |
| `read_truncation_notice` | 44 | `banner`, **`toolUseID`** |
| `date_change` | 36 | `newDate` |
| `command_permissions` | 31 | `allowedTools` |
| `plan_mode` | 29 | `reminderType`, `isSubAgent`, `planFilePath`, `planExists` |
| **`nested_memory`** | 26 | **`path`**, `content`, `displayPath` |
| `plan_mode_exit` | 22 | `planFilePath`, `planExists` |
| `workflow_keyword_request` | 15 | — |
| `ultra_effort_enter` | 15 | `reminderType` |
| **`hook_success`** | 11 | `hookName`, `toolUseID`, `hookEvent`, `content`, `stdout`, `stderr`, `exitCode`, `command`, **`durationMs`** |
| `hook_additional_context` | 11 | `content`, `hookName`, `toolUseID`, `hookEvent` |
| `compact_file_reference` | 7 | `filename`, `displayPath` |
| **`file`** | 6 | **`filename`**, `content`, `displayPath` — an `@`-referenced file |
| `hook_system_message` | 5 | `content`, `hookName`, `toolUseID`, `hookEvent` |
| `plan_mode_reentry` | 3 | `planFilePath` |
| `dynamic_skill` | 2 | `skillDir`, `skillNames`, `displayPath` |
| `plan_file_reference` | 2 | `planFilePath`, `planContent` |
| `invoked_skills` | 2 | `skills` |

Two attachment types matter directly to Polis. `attachment.type == "file"` and
`"nested_memory"` carry `filename`/`path` and are the **only** transcript record
of a file the human referenced with `@` — the hooks agent established that `@`
produces no `Read` tool call, so this is where that signal lives.
`hook_success` carries a measured `durationMs` for Polis's own hook, useful for
self-monitoring.

### 4.4 `system` (n = 904)

Envelope + `subtype`. `isMeta` on 901 (99.7%).

| `subtype` | n | extra fields |
|---|---:|---|
| `turn_duration` | 552 | **`durationMs`**, **`messageCount`** |
| `away_summary` | 210 | `content` |
| `local_command` | 131 | `content`, `level` |
| `informational` | 4 | `content`, `level` (`info` \| `warning`) |
| `compact_boundary` | 3 | `content`, `level`, `logicalParentUuid`, `compactMetadata` |
| `scheduled_task_fire` | 1 | `content` |

`pendingWorkflowCount` (53) and `pendingBackgroundAgentCount` (51) appear on
some system records — a cheap live count of in-flight background work.

`compactMetadata` = `{trigger ("auto"\|"manual"), preTokens, postTokens,
cumulativeDroppedTokens, durationMs, preCompactDiscoveredTools[],
preservedSegment {headUuid, anchorUuid, tailUuid}, preservedMessages}`.

**Compaction breaks the tree.** After a `compact_boundary`, the following
records chain to the boundary record, and the pre-compaction thread is reachable
only through `logicalParentUuid`. Polis must follow `logicalParentUuid` when
present or it will render a compacted session as two unrelated trees.

### 4.5 Flat session sidecars

```
{"type":"mode",            "sessionId":…, "mode":"normal"}
{"type":"permission-mode", "sessionId":…, "permissionMode":"auto"|"plan"|"default"|"acceptEdits"}
{"type":"ai-title",        "sessionId":…, "aiTitle":…}
{"type":"custom-title",    "sessionId":…, "customTitle":…}
{"type":"agent-name",      "sessionId":…, "agentName":…}
{"type":"atis-latch",      "sessionId":…, "atis":…}
{"type":"last-prompt",     "sessionId":…, "leafUuid":…, "lastPrompt":… (98.6%)}
{"type":"queue-operation", "sessionId":…, "operation":"enqueue"|"dequeue"|"remove"|"popAll",
                           "timestamp":…, "content":… (86.6%), "reason":… (0.3%)}
{"type":"bridge-session",  "sessionId":…, "bridgeSessionId":…, "lastSequenceNum":int,
                           "ownerAccountUuid":…, "ownerOrganizationUuid":…}   ← account PII, do not log
{"type":"frame-link",      "sessionId":…, "frameUrl":…, "path":…, "title":…, "timestamp":…}
{"type":"cost-state",      "sessionId":…, "startTime":int, "totalCostUSD":float,
                           "totalDuration":int, "totalAPIDuration":int,
                           "totalAPIDurationWithoutRetries":int, "totalToolDuration":int,
                           "totalLinesAdded":int, "totalLinesRemoved":int,
                           "modelUsage":{…}, "hasUnknownModelCost":bool}
{"type":"file-history-snapshot","messageId":…, "isSnapshotUpdate":bool,
                           "snapshot":{messageId, trackedFileBackups, timestamp}}
{"type":"file-history-delta",  "messageId":…, "snapshotMessageId":…, "trackingPath":…,
                           "timestamp":…, "backup":{backupFileName, version, backupTime, realParentDir}}
```

`cost-state.totalLinesAdded` / `totalLinesRemoved` is a **free, exact,
session-cumulative diff-line counter** — the §7.3 height signal without
reconstructing it from patches. It only appears 3 times in this corpus (it is
written at session end), so treat it as a reconciliation input, not a live one.

`file-history-delta.trackingPath` is a path Claude Code itself is tracking for
undo — a second, independent list of files an agent has touched.

### 4.6 `journal.jsonl` (workflow runs only)

Two record types, no envelope, no timestamp:

```
{"type":"started","key":"v2:<64-hex>","agentId":"a2720d4b074c18aed"}
{"type":"result", "key":"v2:<64-hex>","agentId":"a95f8e060a6a5410b","result": {…} | "…"}
```

`result.result` is an object (389) or a bare string (77) — the agent's
`StructuredOutput` payload or its final message.

40 journals, 504 `started` records; **every one of the 504 `agentId`s has a
sibling `agent-<id>.jsonl`, and every sibling file has a `started` record** —
a perfect bijection, 0 orphans in either direction. 43 agents have `started`
with no `result` (still running or crashed). This makes `journal.jsonl` the
cheapest live liveness signal for a workflow: tail it and you learn agent
start/stop without parsing any transcript.

---

## 5. `message.content` — string vs blocks

| record type | `content` shape | n |
|---|---|---:|
| `assistant` | array | 66 098 (100%) |
| `user` | array | 38 070 (96%) |
| `user` | **bare string** | 1 616 (4%) |

`Content` must be an untagged enum. Assistant content is never a bare string;
user content is, on plain human turns.

Block types, by parent record type:

| block | in `assistant` | in `user` | keys |
|---|---:|---:|---|
| `tool_use` | 37 917 | — | `type`, `id`, `name`, `input`, `caller` (all 100%) |
| `thinking` | 20 100 | — | `type`, `thinking`, `signature` (all 100%) |
| `text` | 8 085 | 164 | `type`, `text` |
| `tool_result` | — | 37 912 | `type`, `tool_use_id`, `content`, `is_error` (49.9%) |
| `image` | — | 30 | `type`, `source` |

`tool_use.caller` is `{"type":"direct"}` in **37 917 / 37 917** blocks. It has
never taken another value here; model it as `Option<Value>` and ignore it.

`tool_result.content` is a string (35 460) or an array (2 452). Array element
types: `text` (4 152), `tool_reference` (622, `{type, tool_name}`), `image` (485).

**One tool_result per user record.** The distribution of tool_result blocks per
user record is `{1: 38 239}` — never 0, never ≥ 2. Parallel tool calls appear as
one `tool_use` block per assistant record's content array, answered by one user
record each. This makes the `toolUseResult` sidecar (which is a single object,
not an array) unambiguous.

---

## 6. Tool inventory and input key sets

62 distinct tool names. **There is no `Task` tool** — the subagent-spawning tool
is named **`Agent`**. There is no `MultiEdit` (consistent with `hooks-schema.md`).
`NotebookEdit` exists as a deferred tool but was never invoked here.

| tool | n | input keys (presence) |
|---|---:|---|
| `Bash` | 16 416 | `command` 100%, `description` 95.8%, `timeout` 10.3%, `run_in_background` 0.7%, `dangerouslyDisableSandbox` 0.0% |
| `Read` | 5 844 | `file_path` 99.8%, `limit` 53.4%, `offset` 50.3%, `pages` 0.0%, **`__unparsedToolInput` 0.2%** |
| `Grep` | 3 336 | `pattern` 100%, `output_mode` 99.8%, `path` 93.5%, `-n` 81.3%, `head_limit` 50.7%, `-C` 27.7%, `-A` 9.5%, `-i` 8.4%, `glob` 7.5%, `-B` 2.5%, `-o` 0.5%, `offset` 0.4%, `context` 0.1%, `multiline` 0.1% |
| `Edit` | 3 000 | `file_path` 100%, `old_string` 100%, `replace_all` 100%, `new_string` 100.0% (one record lacks it) |
| `PowerShell` | 2 137 | `command` 100%, `description` 96.6%, `timeout` 47.1%, `run_in_background` 2.6% |
| `WebFetch` | 1 701 | `url`, `prompt` (both 100%) |
| `WebSearch` | 790 | `query` 100%, `allowed_domains` 3.2% |
| `Write` | 775 | `file_path`, `content` (both 100%) |
| `Glob` | 360 | `pattern` 100%, `path` 24.2%, `head_limit` 0.3% |
| `ToolSearch` | 252 | `query`, `max_results` (both 100%) |
| `Agent` | 169 | `description` 100%, `prompt` 100%, `subagent_type` 99.4%, `run_in_background` 34.3%, `model` 3.0% |
| `TaskUpdate` | 206 | `taskId` 100%, `status` 99.0%, `description` 3.9% |
| `TaskCreate` | 120 | `subject` 99.2%, `description` 99.2%, `activeForm` 92.5% |
| `Workflow` | 45 | `script` 93.3%, `description` 68.9%, `scriptPath`/`resumeFromRunId` 6.7%, `args` 4.4% |
| `Skill` | 34 | `skill` 100%, `args` 32.4% |
| `StructuredOutput` | 368 | **schema is caller-defined — 160+ distinct keys.** Never model it. |
| `mcp__*` | 2 700+ | server-defined |

Two traps:

- **`Read.file_path` is only 99.8%.** Nine records instead carry
  `__unparsedToolInput` — the model emitted malformed JSON for the tool input and
  the harness preserved it verbatim. `SendUserFile` shows the same key once. Any
  code doing `input["file_path"].as_str().unwrap()` will panic on real data.
- The Windows shell tool is `PowerShell`, not `Bash`, on 2 137 calls. Every
  matcher and every "is this a command" branch must accept `Bash|PowerShell`.

Path-bearing tool inputs, corpus-wide: `file_path` 9 599, `path` 3 208,
`filename` 64, `scriptPath` 3. Separator style is backslash 11 606 / forward
1 268 — **both appear**, so normalize before using a path as a map key.

---

## 7. `tool_result` → `tool_use` linkage, errors, and the `toolUseResult` sidecar

### 7.1 The join

`tool_result.tool_use_id` → `tool_use.id` (`toolu_…`). Present on 37 912/37 912
tool_result blocks. The producing assistant record is additionally named by
`user.sourceToolAssistantUUID`, which equals `parentUuid` every time.

Ids are **file-local in practice but must be resolved session-wide**: a nested
subagent's `meta.json` references a `toolu_` id that lives in a *different* file
(§8.3). Build the index over all files of a session, not per file.

### 7.2 Where success/failure lives

Two independent places, and they disagree in coverage:

1. **`tool_result.is_error`** — present on 18 904 of 37 912 blocks (49.9%).
   Values: `false` 17 881, `true` 1 023. **Absent means "not an error"**; there
   is no third state.
2. **`toolUseResult` absence.** For a failing tool the structured sidecar is
   usually replaced by a plain error string, so
   `toolUseResult: string` correlates with `is_error: true` (e.g. `Bash`: 353
   string-shaped sidecars, 353 errors; `Grep`: 25/25; `Read`: 34/34).

Use `is_error.unwrap_or(false)` as the primary signal. Do not infer failure from
a missing sidecar — see §7.4.

### 7.3 `toolUseResult` shapes by tool

Shape overall: object 20 479, array 1 744, **string 1 070**. The string form is
the error path. The array form is MCP tools returning content blocks.

| tool | sidecar fields (of the object form) |
|---|---|
| `Read` | `type` (`text`\|`image`\|`file_unchanged`), `file{filePath, content, numLines, startLine, totalLines, +truncatedByTokenCap, +base64/type/originalSize/dimensions}` |
| `Edit` | **`filePath`**, `oldString`, `newString`, `originalFile` (string\|null), **`structuredPatch`**, `userModified`, `replaceAll`, +`staleRecovered`, +`memdirStamped` |
| `Write` | `type` (`create`\|`update`), **`filePath`**, `content`, **`structuredPatch`**, `originalFile`, `userModified` |
| `Bash` | `stdout`, `stderr`, `interrupted`, `isImage`, `noOutputExpected`, +`backgroundTaskId`, +`timedOutAfterMs`, +**`gitOperation`**, +`returnCodeInterpretation`, +`backgroundCwdHint`, +`persistedOutputPath`/`persistedOutputSize` |
| `PowerShell` | same as `Bash` minus `noOutputExpected` |
| `Glob` | `filenames[]`, `durationMs`, `numFiles`, `truncated`, `totalMatches`, `countIsComplete` |
| `Grep` | `mode` (`content`\|`files_with_matches`\|`count`), `numFiles`, `filenames[]`, +`content`, +`numLines`, +`totalLines`, +`appliedLimit`, +`totalFiles`, +`numMatches`, +`appliedOffset` |
| `Agent` | **`agentId`**, `status`, `resolvedModel`, `prompt`, +async: `isAsync`, `description`, `outputFile`, `canReadOutputFile`; +sync: `agentType`, `content[]`, `totalDurationMs`, `totalTokens`, `totalToolUseCount`, `usage`, **`toolStats`** |
| `Workflow` | `status`, `taskId`, `taskType`, `workflowName`, **`runId`**, `summary`, **`transcriptDir`**, `scriptPath` |
| `WebFetch` | `bytes`, `code`, `codeText`, `result`, `durationMs`, `url` |
| `WebSearch` | `query`, `results[]`, `durationSeconds`, `searchCount` |
| `TaskCreate` / `TaskUpdate` / `TaskStop` / `TaskOutput` | `task{…}` / `success,taskId,updatedFields,statusChange{from,to}` / `message,task_id,task_type,command` / `retrieval_status,task` |
| `Artifact` | `url`, `path`, `title`, `updated`, `version`, `liveSubscription` |

`Agent.toolStats` is a ready-made per-subagent activity summary:
`{readCount, searchCount, bashCount, editFileCount, linesAdded, linesRemoved,
otherToolCount}`.

`Bash.gitOperation` is `{commit:{sha,kind,branch}}` / `{push:…}` / `{branch:…}` —
80 records. The CLI has already classified the git operation for you; Polis
should not re-parse `git` command lines to find commits.

`persistedOutputPath` points into `<session-id>/tool-results/<id>.txt` when the
output was too large to inline. 25 records. If Polis wants the real output it
must read that file.

### 7.4 🚨 `toolUseResult` is missing on most subagent tool results

This is the biggest usability finding in the file.

| file class | sidecar present | sidecar absent |
|---|---:|---:|
| main transcript | 15 542 (100%) | **0** |
| subagent transcript | 7 886 (34.7%) | **14 811 (65.3%)** |

By CLI version: 2.1.229 → 22 888 present / 12 710 absent; 2.1.248 → 325 / 1 529.
It is not correlated with `is_error`, and only weakly with output size. 504 of
643 subagent files have at least one missing sidecar; many are 100% missing
(`agent-ac76486a11551cec6.jsonl`: 91 tool results, 0 sidecars).

Consequences for Polis:

- **Never make `toolUseResult` load-bearing.** File paths, diff line counts and
  exit status must degrade to `tool_use.input` (which is always present) plus
  the `tool_result.content` string.
- The tools most often missing a sidecar are exactly the ones Polis cares about:
  `Bash` 6 171, `Read` 3 234, `Grep` 1 894, `PowerShell` 724, **`Edit` 625**,
  `Write` 185.
- For an `Edit` with no sidecar you get `file_path`, `old_string`, `new_string`
  from the input and nothing else. You can still count `\n` in the two strings to
  approximate the line delta; you cannot get hunk line ranges, so §11.3's
  "overlapping line ranges" tier degrades to "same file" for subagent edits.
  **Use the `PreToolUse` hook for contention, as `hooks-schema.md` recommends** —
  the transcript cannot carry that load for subagents.

---

## 8. SUBAGENT LINKAGE — the critical section

**Verdict: the link is reliable, complete, and fully resolvable — but only if you
read `agent-<id>.meta.json`. The `.jsonl` alone does not name its parent.**

### 8.1 Identifying a subagent record

Three signals, all 100% consistent across 61 239 subagent records:

| signal | subagent files | main files |
|---|---|---|
| lives under `<session-id>/subagents/` | 643 files | — |
| `isSidechain` | `true` on 62 470/62 470 | **`false` on 64 251/64 251** — 0 sidechain records ever appear in a main transcript |
| `agentId` present | 61 239/61 239 at scan time, and **always equals the agent id in the filename** | never present |

So: `isSidechain == true` ⟺ `agentId.is_some()` ⟺ the record is in a subagent
file. Any one of the three is a sufficient discriminator. Subagent records are
**never interleaved into the parent file** — this is a clean file-level split,
which is exactly what a per-thread tailer wants.

Agent id format: `a` + 16 lowercase hex, e.g. `a96cf8a57447af436`.

### 8.2 Each subagent file is its own tree

The first record of all 643 subagent files has `parentUuid: null`. There is **no
uuid-level edge** from a parent record into a subagent file, and none out. Four
separate trees per session is normal.

`sessionId` inside a subagent file equals the **parent session's** id
(61 239/61 239) — it is *not* a distinct id for the subagent. Two different
subagents of the same session therefore have identical `sessionId`. Key threads
by `(sessionId, agentId)`, never by `sessionId` alone.

### 8.3 The authoritative link: `agent-<agentId>.meta.json`

```json
{"agentType":"general-purpose","description":"Read target.txt and return first line",
 "toolUseId":"toolu_01Q3g2QgvKmHqJZ3XMPgmJJ5","spawnDepth":1}
```

643 meta files:

| field | n | % | meaning |
|---|---:|---:|---|
| `agentType` | 643 | 100 | `workflow-subagent` 473, `Explore` 87, `general-purpose` 57, `Plan` 26 |
| `spawnDepth` | 643 | 100 | `1` × 620, `2` × 21, `3` × 2 |
| `description` | 140 | 21.8 | present ⟺ `agentType != workflow-subagent` |
| **`toolUseId`** | 140 | 21.8 | present ⟺ `agentType != workflow-subagent` |
| `parentAgentId` | 23 | 3.6 | present ⟺ `spawnDepth > 1` |
| `model` | 5 | 0.8 | e.g. `"sonnet"` |

**Resolution result, over all 140 directly-spawned subagents:**

```
resolved to an `Agent` tool_use in the MAIN transcript (depth=1)   117
resolved to an `Agent` tool_use in a PARENT SUBAGENT file (depth=2) 21
resolved to an `Agent` tool_use in a PARENT SUBAGENT file (depth=3)  2
UNRESOLVED                                                           0
```

100%, with zero exceptions — provided you index tool_use ids across **every file
in the session**, not just the main transcript. The 23 that are not in the main
file are nested agents, and `parentAgentId` names the file to look in.

### 8.4 The three linkage routes, ranked

**Route 1 — `meta.json.toolUseId` (authoritative, use this).**
`toolUseId` → the `Agent` tool_use block → its record's `uuid` → the parent
thread. Also gives you `agentType`, `spawnDepth`, `parentAgentId`. 140/140.
Works for directly spawned agents only.

**Route 2 — `toolUseResult.agentId` in the parent (authoritative, confirms
Route 1).** The user record answering the `Agent` tool call carries
`toolUseResult.agentId`, which equals the subagent's filename id. Found for
117/140 — the same 117 whose parent is the main transcript; for nested agents the
record lives in the parent subagent file and is found there too. Also carries
`status` (`async_launched` 165 \| `completed` 17), `isAsync`, `resolvedModel`,
`description`, and for sync agents the full `toolStats`/`usage` roll-up.

Async vs sync matters: with `run_in_background: true` (58/169 `Agent` calls) the
result arrives immediately as `async_launched` and the subagent file keeps
growing afterwards. The completion arrives later as a separate
`origin.kind == "task-notification"` user record. A tailer must not treat
`async_launched` as "done".

**Route 3 — workflow agents (503 of 643): directory + `runId`.**
Workflow-spawned agents have **no `toolUseId` and no `parentAgentId`**. They are
linked structurally: they live in `subagents/workflows/wf_<runId>/`, and the
parent's `Workflow` tool result carries `runId` plus an absolute `transcriptDir`.
Verified: **40 run directories on disk, 40 `runId`s in records, 40 matched, 0
disk-only, 0 record-only.** Inside such a run, `journal.jsonl` gives
`started`/`result` per `agentId` with a perfect 504/504 bijection to the files.

**Route 4 — `promptId` (weak, do not rely on it).** A subagent file's `promptId`
appears in the parent main transcript for **616 of 641** files; 25 miss. All 25
misses share a single `promptId` (`0a1524f9-…`), i.e. one turn whose grouping id
never reached the parent file. `promptId` is a good *secondary* grouping key for
"everything this turn produced", but it is not a parent pointer and it is not
100%.

**Also available:** `SubagentStop`'s hook payload carries an authoritative
`agent_transcript_path` + `agent_id` (per `hooks-schema.md`). That is the
lowest-latency route of all and needs no directory scanning — but it only fires
at *stop*. Use the hook for lifecycle, `meta.json` for structure.

### 8.5 🚨 `attributionAgent` is not an agent id

It appears on 37 369 assistant records and looks like an identifier. Its values
are `workflow-subagent` (23 265), `general-purpose` (6 070), `Explore` (5 915),
`Plan` (1 702) — it is the agent **type**, identical to `meta.json.agentType`,
and it never equals `agentId` (0 matches in 36 952 comparisons). Do not use it
to key a thread.

### 8.6 Minimal correct algorithm

```
for each session dir S:
  index := { tool_use.id -> (record.uuid, file, record.agentId) }   # ALL files of S
  for each agent-<id>.jsonl under S/subagents/**:
      meta := read sibling agent-<id>.meta.json          # if missing, thread is unattributed
      if meta.toolUseId:                                  # direct spawn
          parent_record, parent_file, parent_agent := index[meta.toolUseId]
          parent_thread := parent_agent ?? MAIN
      else:                                               # workflow spawn
          runId := path segment matching ^wf_
          parent_thread := MAIN, workflow := runId        # confirm via toolUseResult.runId
      thread_key := (session_id, id)                      # never session_id alone
```

---

## 9. Paths, line counts and diffs (PRD §7.3 and §11.3)

### 9.1 Every field that carries a file path

| where | field | n |
|---|---|---:|
| `tool_use.input` | `file_path` | 9 599 |
| `tool_use.input` | `path` (Grep/Glob dir) | 3 208 |
| `tool_use.input` | `filename` | 64 |
| `toolUseResult` | `filePath` (Edit/Write) | 2 950 |
| `toolUseResult.file` | `filePath` (Read) | 2 522 |
| `toolUseResult` | `filenames[]` (Glob/Grep) | 1 604 |
| `toolUseResult` | `outputFile` (async Agent) | 122 |
| `toolUseResult` | `backgroundCwdHint` | 58 |
| `toolUseResult` | `transcriptDir`, `scriptPath` (Workflow) | 43 each |
| `toolUseResult` | `persistedOutputPath` | 25 |
| `attachment` | `filename` (`file`, `edited_text_file`, `compact_file_reference`) | 300 |
| `attachment` | `path` (`nested_memory`) | 26 |
| record | `file-history-delta.trackingPath` | 906 |
| envelope | `cwd` | every threaded record |

Paths are absolute and mostly backslashed on Windows, but forward slashes do
occur (1 268 tool inputs, 14 sidecar `filePath`s). Normalize to a canonical
logical path — lowercase drive letter, forward slashes — before keying
`FileState` or the claim table, or the same file will occupy two buildings.

### 9.2 Line counts

- **`toolUseResult.file.{numLines, startLine, totalLines}`** — 2 399 records.
  `totalLines` is the file's real length; that plus `sqrt(size)` gives §7.3
  footprint without touching the filesystem.
- **`toolUseResult.structuredPatch`** — 2 436 patches, 2 792 hunks, 38 154 added
  and 12 118 removed lines corpus-wide. Each hunk is
  `{oldStart, oldLines, newStart, newLines, lines[]}`, and every entry in
  `lines[]` keeps its unified-diff prefix (` ` 7 721, `-` 91, `+` 45 in a
  first-three-lines sample). Count `+`/`-` for the height delta; use
  `newStart..newStart+newLines` for the §11.3 line-range overlap test.
- **`toolUseResult.toolStats.{linesAdded, linesRemoved}`** — per-subagent
  roll-up on sync `Agent` results.
- **`cost-state.{totalLinesAdded, totalLinesRemoved}`** — per-session totals.

`structuredPatch` is the only source of *line ranges*. It is absent on 625
subagent `Edit`s (§7.4), so the "same branch, overlapping line ranges" tier of
§11.3 is not achievable from the transcript for subagent edits. That tier needs
the `PreToolUse` hook.

### 9.3 Branch and worktree

`gitBranch` is on every threaded record, so the "different worktrees/branches,
same logical file" severity tier is directly computable from the transcript.
Beware `"HEAD"` (4 463 records) — a detached head, not a branch; two records both
reading `"HEAD"` are not necessarily the same branch.

---

## 10. Timestamps, ordering and tree integrity

**Format.** `"2026-08-24T22:43:22.764Z"` — ISO-8601 UTC, exactly 24 characters,
millisecond precision, always `Z`, always with a `.`. **123 903 / 123 903
records share this shape; there is not a single variant.** Parse with a fixed
format, not a general ISO parser, and you will be both faster and stricter.

**Monotonicity: NO. Records do arrive out of order.**

- **162 of 808 files (20%) contain at least one backwards step.**
- Worst offender: 189 backwards steps in one file. Others: 65, 58, 46, 40.
- Most inversions are sub-second (`…38.965Z → …38.964Z`) and involve
  `attachment` records being flushed slightly after the record they annotate.
  But a 60-second backwards jump was observed
  (`21:51:09.661Z → 21:50:09.120Z`, `assistant`).

**Implication for Polis:** never use `timestamp` as a sequence key. File byte
order is the true append order. Order events by `(file, byte_offset)` and use
`timestamp` only for display and for the ±2s FS-attribution window in §4.3 —
where a 60 s inversion means path-based correlation must not assume the tool
call precedes the write in timestamp order.

**Tree integrity is excellent.** Over 124 262 records with a `uuid`:

| property | result |
|---|---|
| duplicate `uuid` | **0** |
| dangling `parentUuid` (parent not in file) | **0** |
| parent appearing *after* its child in file order | **1** (of 124 262) |
| roots (`parentUuid == null`) per file | 1 × 801 files, 2 × 4, 3 × 1, 0 × 2 |
| distinct `sessionId` per file | exactly 1 in all 808 files |
| main-file basename ≠ its `sessionId` | **0** |

Fan-out: 114 235 records have exactly 1 child, 4 604 have 2, one has 3, one has
4, 5 421 are leaves. So it is *nearly* a linked list — but PRD §4.4 is right that
it is a tree: 4 606 branch points is not zero. Branching comes from message
editing/rewind. **Render the leaf-most chain, not every branch**, or a rewound
session will show duplicated work. `last-prompt.leafUuid` names the live leaf.

Two files have **zero** roots — every record's parent is present but the file
begins mid-chain (a resumed session whose head lives in the ancestor file named
by `session_id`). A parser must not assume a root exists.

### 10.1 One process, several sessions: `bridgeSessionId` and `/clear`

**Measured 2026-09-06 over the 263 transcripts then under `~/.claude/projects`**
(1 unreadable: its path exceeds `MAX_PATH` through a `glob` that does not use the
`\\?\` prefix — §1.3).

`/clear` writes **no ending record**. It abandons the transcript where it stands
and opens a **new file with a new session id**, whose first user record is
`<command-name>/clear</command-name>`. Nothing in the old file says it is over —
which is the whole difficulty in §10's neighbourhood, and the reason
`polis_ingest::live` otherwise has to reason from silence.

The link between the two files is `bridge-session.bridgeSessionId`. It is a
property of the running `claude` **process**, not of the conversation:

| claim | measurement |
|---|---|
| files carrying ≥ 1 `bridge-session` record | 225 / 262 readable |
| `bridgeSessionId` changes within one file | **0** files |
| the record is re-emitted as the session runs | 1–16+ per file; 217 files have one past line 5 |
| byte offset of the **first** one | ≤ 392 in 224 of 225 files; one outlier at 17 458 |
| distinct bridge ids | 114, of which 48 cover more than one session |
| sessions that are the **successor** of another under one id | 107 (longest chain: 9) |
| …of those, beginning with `/clear` | **106 / 107** (the exception opens with `/design`) |
| …where the predecessor's **last** record post-dates the successor's **first** | **0 / 107** |

So the sessions of one bridge id form a strict, non-overlapping sequence, and a
second live session under an id means the first is finished. That is the one
ending this format lets a reader *prove*, and Polis acts on it: ADR-0105,
`polis_ingest::live::LiveTailer::detect_supersession`.

**Two cautions for anyone implementing it.**

* **Order by the first record, never by mtime.** A finished session's file can
  still be written to — the final assistant flush, a `cost-state` at exit — and
  20% of files contain a backwards timestamp step anyway (§10). The first record
  of each file is the only comparison that survives both.
* **The bridge id is readable before the first timestamp is.** `bridge-session`
  is written among the opening sidecars, several records ahead of the first
  threaded one, so there is a window of a few hundred bytes in which a brand-new
  session has an id but no time. A reader that treats "no timestamp" as "oldest"
  will conclude the *new* session was the one that ended.

---

## 11. Redacted sample records

File contents, commands, prose, thinking text and signatures are replaced with
`<field redacted, N chars, M lines>`; the project name is rewritten; the user's
email address does not occur. Structure, ids, key sets and numbers are untouched.

**1. `assistant` issuing a tool call** (envelope + tool_use block):

```json
{"parentUuid":"73268f73-8bf3-4ead-b5cf-6f4de3811fe3","isSidechain":false,
 "message":{"model":"claude-opus-5","id":"msg_011CeNP6iDf9Nj77hnN5GEcE","type":"message",
   "role":"assistant",
   "content":[{"type":"tool_use","id":"toolu_017kDbmBLeznQFdzgq4BANWx","name":"Bash",
     "input":{"command":"<command redacted, 365 chars, 1 lines>",
              "description":"Check existing MCP server config","timeout":30000},
     "caller":{"type":"direct"}}],
   "stop_reason":"tool_use","stop_sequence":null,"stop_details":null,
   "usage":{"input_tokens":2,"cache_creation_input_tokens":99078,"cache_read_input_tokens":0,
            "output_tokens":565,"output_tokens_details":{"thinking_tokens":282},
            "service_tier":"standard",
            "cache_creation":{"ephemeral_1h_input_tokens":99078,"ephemeral_5m_input_tokens":0},
            "inference_geo":"not_available","speed":"standard"},
   "diagnostics":null},
 "requestId":"req_011CeNP6hD92fJ6aff3oEhk1","type":"assistant",
 "uuid":"d915d107-0a03-45c7-9904-2528ff183da3","timestamp":"2026-08-24T22:43:22.764Z",
 "effort":"xhigh","session_id":"65aaa23f-b905-4c2c-8e8c-b4879e9c4ff3","userType":"external",
 "entrypoint":"cli","cwd":"C:\\coding\\demo-app",
 "sessionId":"65aaa23f-b905-4c2c-8e8c-b4879e9c4ff3","version":"2.1.229","gitBranch":"main"}
```

**2. `user` carrying the matching tool result** — note `tool_use_id`,
`is_error: false`, `sourceToolAssistantUUID == parentUuid`, and the `Bash`
sidecar shape:

```json
{"parentUuid":"d915d107-0a03-45c7-9904-2528ff183da3","isSidechain":false,
 "promptId":"6326b6a1-5f89-4093-b911-5f96da31e413","type":"user",
 "message":{"role":"user","content":[
   {"tool_use_id":"toolu_017kDbmBLeznQFdzgq4BANWx","type":"tool_result",
    "content":"<content redacted, 358 chars, 15 lines>","is_error":false}]},
 "uuid":"2a836c10-c5c0-4f3c-a93f-8a0a7856b731","timestamp":"2026-08-24T22:43:52.136Z",
 "toolUseResult":{"stdout":"<stdout redacted, 358 chars, 15 lines>",
   "stderr":"<stderr redacted, 0 chars, 1 lines>","interrupted":false,"isImage":false,
   "noOutputExpected":false},
 "sourceToolAssistantUUID":"d915d107-0a03-45c7-9904-2528ff183da3",
 "session_id":"65aaa23f-b905-4c2c-8e8c-b4879e9c4ff3","userType":"external","entrypoint":"cli",
 "cwd":"C:\\coding\\demo-app","sessionId":"65aaa23f-b905-4c2c-8e8c-b4879e9c4ff3",
 "version":"2.1.229","gitBranch":"main"}
```

**3. An `Edit` result — the §7.3 / §11.3 payload.** `structuredPatch` hunk
geometry is intact; only the line text is replaced:

```json
{"parentUuid":"05e81420-1aac-4813-8058-aaa160582935","isSidechain":false,
 "promptId":"839ca239-56d5-4ee6-95fc-041e17fbea50","type":"user",
 "message":{"role":"user","content":[
   {"tool_use_id":"toolu_01VtXSiuPLgtyXFqrVjCt3xR","type":"tool_result",
    "content":"<content redacted, 181 chars, 1 lines>"}]},
 "uuid":"4fdf59b4-ba63-4631-9978-76ae77450f89","timestamp":"2026-08-24T16:41:40.905Z",
 "toolUseResult":{
   "filePath":"C:\\coding\\demo-app\\src\\components\\WorkspaceContainer\\WorkspaceContainer.jsx",
   "oldString":"<oldString redacted, 188 chars, 3 lines>",
   "newString":"<newString redacted, 156 chars, 3 lines>","originalFile":null,
   "structuredPatch":[{"oldStart":501,"oldLines":7,"newStart":501,"newLines":7,
     "lines":[" REDACTED"," REDACTED"," REDACTED","-REDACTED","+REDACTED",
              " REDACTED"," REDACTED"," REDACTED"]}],
   "userModified":false,"replaceAll":false},
 "sourceToolAssistantUUID":"05e81420-1aac-4813-8058-aaa160582935",
 "session_id":"cb566e5d-73ec-40eb-a92e-2b4b8f3c7581","userType":"external","entrypoint":"cli",
 "cwd":"C:\\coding\\demo-app","sessionId":"cb566e5d-73ec-40eb-a92e-2b4b8f3c7581",
 "version":"2.1.229","gitBranch":"HEAD"}
```

**4. The parent record of a subagent spawn** — `toolUseResult.agentId` is the
filename of the subagent transcript; note `status: "completed"` (synchronous)
and the `toolStats` roll-up:

```json
{"parentUuid":"dc920a79-fde2-4268-90f6-a41804ee066b","isSidechain":false,
 "promptId":"09a624c0-ca45-4e72-aeef-0ce917a44d18","type":"user",
 "message":{"role":"user","content":[
   {"tool_use_id":"toolu_01Q3g2QgvKmHqJZ3XMPgmJJ5","type":"tool_result",
    "content":[{"type":"text","text":"<text redacted, 14 chars, 1 lines>"},
               {"type":"text","text":"<text redacted, 195 chars, 4 lines>"}]}]},
 "uuid":"093bc686-e0f2-4f89-8577-8fd9ab1a7bd9","timestamp":"2026-09-01T17:38:58.051Z",
 "toolUseResult":{"status":"completed","prompt":"<prompt redacted, 109 chars, 1 lines>",
   "agentId":"a96cf8a57447af436","agentType":"general-purpose",
   "resolvedModel":"claude-haiku-4-5-20251001","totalDurationMs":5118,"totalTokens":17824,
   "totalToolUseCount":1,
   "toolStats":{"readCount":1,"searchCount":0,"bashCount":0,"editFileCount":0,
                "linesAdded":0,"linesRemoved":0,"otherToolCount":0}},
 "sourceToolAssistantUUID":"dc920a79-fde2-4268-90f6-a41804ee066b","userType":"external",
 "entrypoint":"sdk-cli","cwd":"C:\\Users\\user\\...\\probe2",
 "sessionId":"005d9938-2b44-45f6-87ac-4cddac3b0d6b","version":"2.1.248","gitBranch":"HEAD"}
```

**5. The subagent's own first assistant record** — `isSidechain: true`,
`agentId` matching the filename, `sessionId` still the *parent's*,
`attributionAgent` holding the agent **type**:

```json
{"parentUuid":"0f763bf8-7607-4d5d-9f2a-dc92640b2ecb","isSidechain":true,
 "agentId":"a96cf8a57447af436",
 "message":{"model":"claude-haiku-4-5-20251001","id":"msg_011Ced8Mmz6kC7r4sUMfjPD5",
   "type":"message","role":"assistant",
   "content":[{"type":"thinking","thinking":"<thinking redacted, 0 chars, 1 lines>",
               "signature":"<signature redacted, 1128 chars, 1 lines>"}],
   "stop_reason":null,"stop_sequence":null,"stop_details":null,
   "usage":{"input_tokens":10,"cache_creation_input_tokens":16835,
            "cache_read_input_tokens":0,"output_tokens":3,"service_tier":"standard",
            "inference_geo":"not_available"},
   "diagnostics":null},
 "requestId":"req_011Ced8MmEi6ppDRGfNZdaNG","attributionAgent":"general-purpose",
 "type":"assistant","uuid":"f31d965f-7a30-47e8-82fa-d86a24c50ddc",
 "timestamp":"2026-09-01T17:38:56.264Z","userType":"external","entrypoint":"sdk-cli",
 "cwd":"C:\\Users\\user\\...\\probe2","sessionId":"005d9938-2b44-45f6-87ac-4cddac3b0d6b",
 "version":"2.1.248","gitBranch":"HEAD"}
```

**6. A flat sidecar, for contrast** — no uuid, no timestamp, no envelope:

```json
{"type":"mode","mode":"normal","sessionId":"65aaa23f-b905-4c2c-8e8c-b4879e9c4ff3"}
```

---

## 12. Serde modelling recommendation

The full reference lives in the scratch crate
`…/scratchpad/jsonlref/src/lib.rs` and its two test files; it compiles on
rustc 1.94.0 and its tests pass, including a corpus test that parses all
153 613 local records with zero failures. Dependencies actually compiled:

```toml
serde      = { version = "1.0.229", features = ["derive"] }
serde_json = { version = "1.0.151", features = ["preserve_order"] }
```

### 12.1 The record enum

An **internally tagged enum on `"type"` with `#[serde(other)] Unknown`**. Newtype
variants (boxed, because `AssistantRecord` is large) for the four threaded types;
struct variants for the flat sidecars, which have no fields worth sharing.

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum Record {
    #[serde(rename = "user")]      User(Box<UserRecord>),
    #[serde(rename = "assistant")] Assistant(Box<AssistantRecord>),
    #[serde(rename = "attachment")] Attachment(Box<AttachmentRecord>),
    #[serde(rename = "system")]    System(Box<SystemRecord>),
    #[serde(rename = "mode")] Mode { #[serde(rename="sessionId")] session_id: String, mode: String },
    // … 14 more sidecars …
    #[serde(other)] Unknown,
}
```

`Box` the four big variants: without it `Record` is ~600 bytes and every
`Vec<Record>` pays for it.

### 12.2 Where `#[serde(flatten)]` is required

| struct | flatten | why |
|---|---|---|
| `UserRecord` / `AssistantRecord` / `AttachmentRecord` / `SystemRecord` | `#[serde(flatten)] env: Envelope` | the 14 envelope fields are shared and must not be duplicated 4× |
| the same four | `#[serde(flatten)] extra: serde_json::Map<String, Value>` | **the PRD §4.4 catch-all.** New fields appear between CLI versions (`queueSkipAttachments` and `sdk-cli` both appeared inside this corpus). Keeping them means schema drift is *observable* rather than silent. |
| `Message` | `#[serde(flatten)] extra` | `context_management`, `container` appear on <0.1% of records |
| `Attachment` | `#[serde(flatten)] fields` | 25 payload shapes; do not model them as an enum |
| `Usage` | `#[serde(flatten)] extra` | `iterations`, `speed`, `output_tokens_details` are version-dependent |

Do **not** flatten `toolUseResult` into a typed enum keyed on the tool name.
There are 62 tools, the shape is not tagged, and it is a bare string on the error
path. Keep it as `Option<Value>` on the record and project it lazily into a
permissive `ToolOutcome` struct (`#[serde(default, rename_all = "camelCase")]`,
every field `Option`/`Vec`) via
`serde_json::from_value(v).unwrap_or_default()`.

### 12.3 The rename trap that a corpus test caught

Every threaded-record struct needs `#[serde(rename_all = "camelCase")]`. Without
it, `toolUseResult`, `promptId`, `requestId`, `agentId` and `isMeta` all silently
deserialize to `None` — and because they are all `Option` with `#[serde(default)]`
and there is a `flatten` catch-all, **serde reports no error at all**. The
corpus test passed with `bad_shape = 0` while every interesting field was empty.

Guard against it with assertions on population counts, not just on parse success:

```rust
assert!(tool_use_results > 20_000, "toolUseResult never deserialized");
assert!(prompt_ids      > 30_000, "promptId never deserialized");
```

Also note `sourceToolAssistantUUID` needs an **explicit** `rename` — camelCase
of `source_tool_assistant_uuid` is `sourceToolAssistantUuid`, which is wrong.

### 12.4 The defensive-parsing contract (PRD §4.4)

Three outcomes per line, no `Result` escaping to the caller, no panic:

```rust
pub enum LineOutcome {
    Record(Record),
    Drift { raw_type: Option<String>, value: Value },  // valid JSON, unmodelled
    Corrupt,                                           // not valid JSON
}

pub fn parse_line(line: &str, stats: &mut ParseStats) -> LineOutcome {
    let line = line.trim();
    if line.is_empty() { return LineOutcome::Corrupt }
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => { stats.bad_json += 1; return LineOutcome::Corrupt }   // torn tail append
    };
    let raw_type = value.get("type").and_then(Value::as_str).map(str::to_owned);
    match serde_json::from_value::<Record>(value.clone()) {
        Ok(Record::Unknown) => { stats.unknown_type += 1;
            *stats.unknown_types_seen.entry(raw_type.clone().unwrap_or("<none>".into())).or_default() += 1;
            LineOutcome::Drift { raw_type, value } }
        Ok(r)  => { stats.ok += 1; LineOutcome::Record(r) }
        Err(_) => { stats.bad_shape += 1; LineOutcome::Drift { raw_type, value } }
    }
}
```

Rules this encodes:

1. **A bad line advances the cursor and increments a counter. It never returns
   `Err` and never aborts the file.** Zero bad lines exist today (0/153 613), but
   a *tailer* will routinely see a half-written final line — the writer appends
   without an atomic rename.
2. **Only read up to the last `\n`.** Never parse a trailing partial line; keep
   the byte offset of the last newline as the resume point.
3. `bad_json + bad_shape + unknown_type` is the **schema-drift counter for the
   status bar** that PRD §16 asks for. Surface `unknown_types_seen` and the set
   of distinct `version` strings; a new CLI version is the leading indicator.
4. **Never `unwrap()` a tool input.** `Read.file_path` is absent on 9 records
   (`__unparsedToolInput` instead).
5. `unknown_type` must never be treated as an error — 15 of the 19 known types
   are cosmetic sidecars, and new ones are cheap to ignore.

### 12.5 Fuzzing (PRD §16)

Seed the `cargo-fuzz` corpus from `tests/fixtures/transcripts/*.jsonl` (§13),
one line per input. The invariant is total: `parse_line` returns for every
possible `&str`. The interesting mutations are truncation (torn append), a
`"type"` whose value is a number or object rather than a string, deeply nested
`input`/`toolUseResult` objects, and `parentUuid` cycles.

---

## 13. Parser fixtures checked in

`C:\coding\agentolis\tests\fixtures\transcripts\` — redacted with
`…/scratchpad/redact.py`. Verified: 0 email addresses, 0 secret-shaped strings,
0 original project names, every line valid JSON, and every structural linkage
preserved.

| file | records | bytes | what it exercises |
|---|---:|---:|---|
| `main-session-mixed.jsonl` | 94 | 86 688 | 10 record types (`user` `assistant` `attachment` `system` `mode` `permission-mode` `bridge-session` `ai-title` `last-prompt` `file-history-snapshot`), `Bash` + `ToolSearch` tool calls, `thinking`/`text`/`tool_use`/`tool_result` blocks, `toolUseResult` present, `system/turn_duration` |
| `main-session-with-subagent.jsonl` | 31 | 36 619 | an `Agent` spawn and its `toolUseResult.agentId`, `entrypoint: "sdk-cli"`, `atis-latch`/`queue-operation` sidecars, `tool_result.content` as a **block array** |
| `subagent-a96cf8a57447af436.jsonl` | 8 | 16 308 | `isSidechain: true`, `agentId` on every record, `parentUuid: null` root, parent's `sessionId` |
| `subagent-a96cf8a57447af436.meta.json` | — | 163 | the `toolUseId` → `Agent` tool_use link |

The three files form one complete, verifiable linkage chain:

```
main-session-with-subagent.jsonl
  assistant uuid dc920a79-…  ── tool_use id toolu_01Q3g2QgvKmHqJZ3XMPgmJJ5, name "Agent"
                                        │                    ▲
  user      uuid 093bc686-…  ── toolUseResult.agentId ───────┼──── a96cf8a57447af436
                                                             │
subagent-a96cf8a57447af436.meta.json ── toolUseId ───────────┘
                                        agentType "general-purpose", spawnDepth 1
subagent-a96cf8a57447af436.jsonl     ── every record agentId = a96cf8a57447af436,
                                        isSidechain = true, sessionId = 005d9938-… (the PARENT's)
```

A parser test should assert exactly that chain; if it holds, §8 is implemented.

---

## 14. PRD corrections

### 14.1 §4.4's path is one level too shallow for subagents

The PRD says sessions live at `~/.claude/projects/<munged-cwd>/<session-id>.jsonl`
and stops there. Subagent transcripts — 643 of the 811 files on this machine,
79% — live at
`<munged-cwd>/<session-id>/subagents/[workflows/wf_<runId>/]agent-<agentId>.jsonl`,
a variable-depth path. **Fix:** state both layouts, and specify that the tailer
watches the `<session-id>/` sidecar directory for files that appear at runtime.

### 14.2 §4.4 says "records chained by `parentUuid` — so a session is a tree"; that is true but incomplete

It is a tree *per file*, and a session is **several disjoint trees**: the main
transcript plus one rooted tree per subagent, each with `parentUuid: null` at its
head and **no uuid edge to its parent**. 25% of lines have no `uuid` at all.
**Fix:** "a session is a forest — one tree per transcript file — plus a set of
flat, unthreaded session-sidecar records. Cross-file parenthood is carried by
`agent-<id>.meta.json`, not by `parentUuid`."

### 14.3 §4.4's implied `<munged-cwd>` rule is right; the hash detail is not stated

Confirmed exactly, including the 200-char limit. But the hash is taken over the
**original** cwd, not the munged string, and it is a JS 32-bit `(h<<5)-h+c` over
UTF-16 code units rendered as base-36 — three details that will produce a wrong
directory name if guessed. **Fix:** paste the algorithm. Also note the key can be
**overridden** by the CLI, so Polis must discover directories as well as compute
them.

### 14.4 🚨 The transcript cannot supply §11.3's line-range contention tier for subagents

`toolUseResult` — the only source of `structuredPatch` line ranges — is **absent
on 65% of subagent tool results** (14 811 of 22 697), including 625 `Edit`s. This
is not a rare edge case; whole subagent files have zero sidecars. **Fix:** §11.3
already prefers `PreToolUse` claims; make that mandatory rather than a fallback,
and state that JSONL-derived contention degrades from "overlapping line ranges"
to "same file" whenever the record came from a subagent.

### 14.5 §7.3 building height has a cheaper source than the PRD implies

"Height ∝ uncommitted diff lines" can be read directly:
`structuredPatch` gives exact `+`/`-` counts per edit (38 154 / 12 118 corpus-
wide), `toolStats.{linesAdded,linesRemoved}` gives a per-subagent roll-up, and
`cost-state.{totalLinesAdded,totalLinesRemoved}` gives a per-session total. And
`toolUseResult.file.totalLines` gives file length for the footprint without a
`stat`. **Fix:** name these fields in §7.3 so nobody re-derives them by diffing
the working tree.

### 14.6 §4.3's ±2s attribution window is unsafe against transcript timestamps

Timestamps are **not monotonic**: 20% of files contain a backwards step, and one
observed jump was **60 seconds**. **Fix:** state that JSONL timestamps are for
display only, that event order is file byte order, and that the ±2s FS
correlation window must be anchored on the hook/OTel clock (which is emitted at
the moment of the call) rather than on the transcript's `timestamp`.

### 14.7 The subagent-spawning tool is `Agent`, not `Task`

No tool named `Task` exists in 37 917 tool calls. `Agent` (169 calls) spawns
subagents; `TaskCreate`/`TaskUpdate`/`TaskGet`/`TaskOutput`/`TaskStop` are the
unrelated to-do list. Any matcher or attribution rule written against `Task` will
match nothing. (Consistent with `hooks-schema.md`'s finding that `MultiEdit`
does not exist either.)

### 14.8 🚨 Windows `MAX_PATH`: transcript paths can exceed 260 characters

A cwd deep enough to trip the 200-char cap yields a 207-char directory and a
282-char transcript path, on a machine where `LongPathsEnabled = 0` (§1.3).
Rust `std::fs` copes; Python does not; `notify` is untested. **Fix:** add to §4.3
and §4.4 that all path handling on Windows must stay in `std::path::Path` and
that any external tool invoked on a transcript path needs a `\\?\` prefix. Also
add a startup self-check that reads the discovered transcript before advertising
the session as watchable.

### 14.9 `attributionAgent` looks like a thread id and is not one

It holds the agent **type** (`workflow-subagent`, `general-purpose`, `Explore`,
`Plan`) and never equals `agentId` (0 / 36 952). The thread key is
`(sessionId, agentId)`; `sessionId` alone is *shared by every subagent of a
session*, so it cannot key a thread either.

---

## 15. Method

- **Corpus scans.** Python over `os.walk(~/.claude/projects)`; per-type key
  unions with presence counts, value-type histograms, block/tool profiling,
  cross-file id resolution. Scripts and outputs in
  `…/scratchpad/scan{1..10}.py`, `scan2b.out`, `scan5b.out`, `layout.out`,
  `link.out`, `link2.out`, `paths.out`.
- **Binary extraction.** `grep -a -o -E` over the 227 MB `claude.exe` for the
  project-key function, the `v4` hash, and the transcript path resolver.
- **Live experiment.** Created a 218-character directory, ran
  `claude --print --model haiku`, and compared the resulting project directory
  name against the prediction. Exact match including the `w6jyaf` suffix. The
  same experiment produced the 282-char transcript path used in §1.3.
- **Model verification.** `cargo test` in `…/scratchpad/jsonlref`: 7 unit tests
  plus a corpus test that parses all 864 files / 153 613 records
  (`ok = 153 613, drift = 0, bad_json = 0, bad_shape = 0, unknown_type = 0`), a
  fixture test asserting the subagent linkage chain, and a long-path test that
  opens the >`MAX_PATH` transcript with plain `std::fs`.
- **Re-verification.** The structural claims that came from mid-run scans
  (one tool_result per user record; `toolUseResult` presence by file class;
  `isSidechain` by file class) were re-measured against the grown corpus before
  publication and all held.
- **Redaction verification.** Regex sweep of every fixture for email addresses,
  `sk-`/`ghp_`/`AKIA`/`AIza`/`Bearer`/JWT shapes and the original project names:
  zero hits.
