# Verified: Claude Code OpenTelemetry export schema (Channel A)

**Status:** verified 2026-09-01 against the live monitoring reference **and against real captured traffic**.
**Primary source:** `https://code.claude.com/docs/en/monitoring-usage` (raw markdown fetched from
`https://code.claude.com/docs/en/monitoring-usage.md`, 139,812 bytes, cached at
`scratchpad/otel.md`).
**Ground truth:** two live `claude -p` sessions exported to a purpose-built `tonic` +
`opentelemetry-proto` OTLP/gRPC receiver on this machine. Captures:
`scratchpad/otlp-probe/real.log` (logs+metrics), `cap.log` (logs+metrics, subagent run),
`trace.log` (logs+traces, subagent run).
**Local Claude Code version:** `2.1.248`. **Target:** `x86_64-pc-windows-msvc`, Windows 11 Pro 26200.

> **Beta stability disclaimer.** The docs describe OTel support as beta and the traces signal
> explicitly as "Traces (beta)". Attribute sets are versioned per release — the reference is dense
> with "Requires Claude Code v2.1.2xx or later" and "Before v2.1.2xx, Claude Code reported…" notes.
> **Treat every key in this file as version-specific, not a contract.** §9 specifies the defensive
> parsing Polis must implement.

**Reading rule for this document:**

- **[OBSERVED]** — I saw this on the wire in a capture on this machine. Highest confidence.
- **[DOCUMENTED]** — stated in the live reference; not exercised by my captures.
- Where the two disagree, **observed wins** and the disagreement is called out explicitly.

Where the PRD conflicts with this file, this file wins. See [§10 PRD corrections](#10-prd-corrections).

---

## 1. Environment variables

### 1.1 Master switch and exporter selection

| Variable | Accepted values | Default | Notes |
|---|---|---|---|
| `CLAUDE_CODE_ENABLE_TELEMETRY` | `1` | unset (off) | **Required.** Nothing exports without it. |
| `OTEL_METRICS_EXPORTER` | `otlp`, `prometheus`, `console`, `none` (comma-separated) | unset (off) | |
| `OTEL_LOGS_EXPORTER` | `otlp`, `console`, `none` (comma-separated) | unset (off) | Carries the **events**. This is Polis's main channel. |
| `OTEL_TRACES_EXPORTER` | `otlp`, `console`, `none` | unset (off) | Beta. Also requires `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1`. |
| `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA` | `1` | unset (off) | Enables span tracing. `ENABLE_ENHANCED_TELEMETRY_BETA` is also accepted. |

### 1.2 Endpoint / protocol

| Variable | Accepted values | Default | Notes |
|---|---|---|---|
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `grpc`, `http/protobuf`, `http/json` | **none** | Claude Code has **no default protocol** — you must set this or the per-signal variant, or the `otlp` exporter does nothing. |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | URL | — | All signals. For gRPC use `http://127.0.0.1:4317` (no path). |
| `OTEL_EXPORTER_OTLP_{METRICS,LOGS,TRACES}_PROTOCOL` | as above | inherits generic | Per-signal override. |
| `OTEL_EXPORTER_OTLP_{METRICS,LOGS,TRACES}_ENDPOINT` | URL | inherits generic | Per-signal override. For HTTP these take the full signal path, e.g. `…:4318/v1/logs`. |
| `OTEL_EXPORTER_OTLP_HEADERS` | `k=v,k2=v2` | — | Auth headers. |
| `OTEL_EXPORTER_OTLP_{METRICS,LOGS,TRACES}_HEADERS` | `k=v` | — | **Merged with**, not replacing, the generic headers. |
| `OTEL_EXPORTER_OTLP_CERTIFICATE` / `_CLIENT_CERTIFICATE` / `_CLIENT_KEY` | path | — | mTLS. Per-signal `_METRICS_CLIENT_KEY` etc. also exist. |

### 1.3 Export cadence

| Variable | Accepted values | Default | Notes |
|---|---|---|---|
| `OTEL_METRIC_EXPORT_INTERVAL` | ms | **60000** | |
| `OTEL_LOGS_EXPORT_INTERVAL` | ms | **5000** | |
| `OTEL_TRACES_EXPORT_INTERVAL` | ms | **5000** | |

### 1.4 Content gates (privacy)

| Variable | Accepted values | Default | Effect |
|---|---|---|---|
| `OTEL_LOG_USER_PROMPTS` | `1` | disabled | Include prompt text. Otherwise `prompt=<REDACTED>` **[OBSERVED]**. |
| `OTEL_LOG_ASSISTANT_RESPONSES` | `1`, `0` | disabled | Include response text. **When unset it falls back to `OTEL_LOG_USER_PROMPTS`** — set it to `0` explicitly to keep responses redacted while prompts are on. |
| `OTEL_LOG_TOOL_DETAILS` | `1` | disabled | **The one Polis needs.** Enables `tool_input` / `tool_parameters` on tool events and `file_path` / `full_command` on tool spans. |
| `OTEL_LOG_TOOL_CONTENT` | `1` | disabled | Tool input/output **bodies** as span events. Requires tracing. Polis does not need this. |
| `OTEL_LOG_RAW_API_BODIES` | `1`, `file:<dir>` | disabled | Full Messages API request/response JSON, i.e. the entire conversation history. **Polis must never set this.** |
| `CLAUDE_CODE_OTEL_CONTENT_MAX_LENGTH` | int (UTF-16 code units) | **61440** (60 KB) | Truncation limit for content-bearing attributes. |

### 1.5 Temporality and cardinality

| Variable | Accepted values | Default | Notes |
|---|---|---|---|
| `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE` | `delta`, `cumulative` | **`delta`** | See §4 — this materially changes how Polis accumulates counters. |
| `OTEL_METRICS_INCLUDE_SESSION_ID` | `true`/`false` | **true** | Keep true; Polis keys on `session.id`. |
| `OTEL_METRICS_INCLUDE_VERSION` | `true`/`false` | **false** | Set `true` to get `app.version` as an attribute. |
| `OTEL_METRICS_INCLUDE_ENTRYPOINT` | `true`/`false` | **false** | Set `true` to get `app.entrypoint` (`cli`, `sdk-cli`, `sdk-ts`, `sdk-py`, `claude-vscode`). |
| `OTEL_METRICS_INCLUDE_ACCOUNT_UUID` | `true`/`false` | **true** | Controls `user.account_uuid` + `user.account_id`. **Polis should set `false`** (§8). |
| `OTEL_METRICS_INCLUDE_RESOURCE_ATTRIBUTES` | `true`/`false` | **true** | Controls whether `OTEL_RESOURCE_ATTRIBUTES` keys ride along on datapoints. |
| `OTEL_RESOURCE_ATTRIBUTES` | `k=v,k2=v2` | — | Custom keys. **Never override standard attributes** — on collision the built-in value wins. |
| `CLAUDE_CODE_OTEL_HEADERS_HELPER_DEBOUNCE_MS` | ms | 1740000 (29 min) | Dynamic-header refresh. Not relevant to a local receiver. |

### 1.6 The block Polis should inject

Verified working end-to-end on this machine:

```sh
CLAUDE_CODE_ENABLE_TELEMETRY=1
CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1      # REQUIRED for subagent attribution — see §7(b)
OTEL_LOGS_EXPORTER=otlp
OTEL_METRICS_EXPORTER=otlp
OTEL_TRACES_EXPORTER=otlp                  # REQUIRED for subagent attribution — see §7(b)
OTEL_EXPORTER_OTLP_PROTOCOL=grpc
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317
OTEL_LOGS_EXPORT_INTERVAL=1000
OTEL_TRACES_EXPORT_INTERVAL=1000
OTEL_METRIC_EXPORT_INTERVAL=10000
OTEL_LOG_TOOL_DETAILS=1
OTEL_METRICS_INCLUDE_ACCOUNT_UUID=false    # privacy, see §8
# Deliberately NOT set: OTEL_LOG_USER_PROMPTS, OTEL_LOG_ASSISTANT_RESPONSES,
#                       OTEL_LOG_TOOL_CONTENT, OTEL_LOG_RAW_API_BODIES
```

**Three additions to the PRD's block**: `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA`, `OTEL_TRACES_EXPORTER`,
`OTEL_TRACES_EXPORT_INTERVAL`. Without them Polis has **no way to attribute a tool call to a
subagent** (§7b). This is the single most consequential finding in this document.

---

## 2. Transport and wire shape [OBSERVED]

- gRPC on the standard OTLP services: `opentelemetry.proto.collector.logs.v1.LogsService/Export`,
  `…metrics.v1.MetricsService/Export`, `…trace.v1.TraceService/Export`.
- Claude Code **connects lazily and reconnects**; it tolerates the receiver being absent. Exports
  are dropped silently by the client when nothing is listening.
- Batches are modest. Set `max_decoding_message_size` generously anyway (I used 16 MiB).
- Events arrive on the **logs** service as `LogRecord`s. **The event name is in the record `body`
  as a plain string**, e.g. `body="claude_code.tool_result"`, *and* duplicated in the
  `event.name` attribute without the prefix, e.g. `event.name=tool_result`.
  The OTLP 1.7 `LogRecord.event_name` field (field 12) was **empty on every record** —
  do not key on it.
- `severity_number` was `SEVERITY_NUMBER_UNSPECIFIED` (0) and `severity_text` empty on every
  record. **Severity carries no information; do not filter on it.**

### Instrumentation scope

| Signal | Scope name | Scope version |
|---|---|---|
| Logs/events | `com.anthropic.claude_code.events` | `2.1.248` (tracks the CLI version) |
| Metrics | `com.anthropic.claude_code` | *(empty)* |

The logs scope version is a free, reliable **CLI version** signal even when
`OTEL_METRICS_INCLUDE_VERSION=false`.

### Resource attributes [OBSERVED]

Identical on logs and metrics:

```
host.arch=amd64
os.type=windows
os.version=10.0.26200
service.name=claude-code
service.version=2.1.248
```

`service.name` is always `claude-code` — usable to reject foreign OTLP traffic hitting port 4317.

---

## 3. Standard attributes (on every event and every metric datapoint) [OBSERVED]

| Attribute | Type | Observed value shape | Controlled by |
|---|---|---|---|
| `session.id` | string (UUID v4) | `005d9938-2b44-45f6-87ac-4cddac3b0d6b` | `OTEL_METRICS_INCLUDE_SESSION_ID` (default true) |
| `user.id` | string (64-hex) | anonymous install id from `~/.claude.json` | always |
| `user.email` | string | **real email address** | always when available |
| `user.account_uuid` | string (UUID) | | `OTEL_METRICS_INCLUDE_ACCOUNT_UUID` (default true) |
| `user.account_id` | string | `user_01KFVLC…` (Anthropic admin-API tagged form) | `OTEL_METRICS_INCLUDE_ACCOUNT_UUID` |
| `organization.id` | string (UUID) | | always when available |
| `terminal.type` | string | `windows-terminal` (also `iTerm.app`, `vscode`, `cursor`, `tmux`) | always when detected |
| `app.version` | string | not emitted by default | `OTEL_METRICS_INCLUDE_VERSION` (default **false**) |
| `app.entrypoint` | string | not emitted by default | `OTEL_METRICS_INCLUDE_ENTRYPOINT` (default **false**) |

**Gateway sessions [DOCUMENTED]:** under a Claude apps gateway, `user.id` becomes the IdP subject,
`user.groups` appears (comma-separated), and `identity.source=gateway-oidc` is stamped. Gateway
identity is applied *last* and overrides `user.*` set via `OTEL_RESOURCE_ATTRIBUTES`.

### Events-only additions (never on metrics — unbounded cardinality)

| Attribute | Type | Notes |
|---|---|---|
| `prompt.id` | string (UUID v4) | **The correlation key.** See §7(c). |
| `event.name` | string | Un-prefixed event name. |
| `event.timestamp` | string (ISO 8601, ms, `Z`) | e.g. `2026-09-01T17:38:50.290Z` |
| `event.sequence` | int as string | **Monotonic from 0 per session.** The reliable total order. |
| `workflow.run_id` | string `wf_…` | Only on API/tool events from a Workflow tool run. |
| `workflow.name` | string | `custom` unless `OTEL_LOG_TOOL_DETAILS=1`. |
| `workspace.host_paths` | string array | Desktop app only. |

---

## 4. Metrics

### 4.1 Aggregation temporality — **DELTA** [OBSERVED]

Every metric decoded from the live capture carried
`aggregation_temporality = AGGREGATION_TEMPORALITY_DELTA (1)`. All eight are **monotonic Sums**
(`is_monotonic = true`), i.e. counters. There are **no gauges and no histograms**.

**What this means for Polis:** each export carries the **increment since the previous export**, not
a running total. Polis must **accumulate** (`total += value`) per attribute-set. Treating a delta
stream as cumulative (e.g. `total = value`) silently under-reports every counter to "whatever
happened in the last 10 s". This is configurable — a user could set
`OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE=cumulative` — so **Polis must read the
`aggregation_temporality` field off the wire and branch on it**, never assume.

### 4.2 The eight metrics

All are `Sum` / monotonic / DELTA. All carry the §3 standard attributes plus those listed.

| # | Metric | Unit | Type | Additional attributes |
|---|---|---|---|---|
| 1 | `claude_code.session.count` | *(none)* | counter | `start_type`: `fresh` \| `resume` \| `continue` \| `agents_view` **[OBSERVED `fresh`]** |
| 2 | `claude_code.lines_of_code.count` | *(none)* | counter | `type`: `added` \| `removed`; `model` **[OBSERVED both]** |
| 3 | `claude_code.pull_request.count` | *(none)* | counter | *(standard only)* **[DOCUMENTED]** |
| 4 | `claude_code.commit.count` | *(none)* | counter | *(standard only)* **[DOCUMENTED]** |
| 5 | `claude_code.cost.usage` | `USD` | counter | `model`, `query_source`, `speed`, `effort`, `agent.name`, `skill.name`, `plugin.name`, `marketplace.name`, `mcp_server.name`, `mcp_tool.name` **[OBSERVED `model`, `query_source`, `effort`]** |
| 6 | `claude_code.token.usage` | `tokens` | counter | `type`: `input` \| `output` \| `cacheRead` \| `cacheCreation`; plus every attribute of the cost counter **[OBSERVED all four `type` values]** |
| 7 | `claude_code.code_edit_tool.decision` | *(none)* | counter | `tool_name`: `Edit` \| `Write` \| `NotebookEdit`; `decision`: `accept` \| `reject`; `source`; `language` **[OBSERVED `tool_name=Write decision=accept source=config language=Plain text`]** |
| 8 | `claude_code.active_time.total` | `s` | counter | `type`: `user` (keyboard) \| `cli` (tool exec + response gen) **[OBSERVED `cli`]** |

Note the `type` value casing on the token counter is **camelCase** (`cacheRead`, `cacheCreation`),
not snake_case — unlike the corresponding `api_request` event attributes
(`cache_read_tokens`, `cache_creation_tokens`).

**Prometheus caveat [DOCUMENTED]:** when `prometheus` is the *only* exporter, the `USD`, `tokens`
and `s` units are omitted so the scrape stays valid. Irrelevant to Polis (gRPC/OTLP).

### 4.3 `query_source` has two different vocabularies [OBSERVED — docs do not flag this]

This is a real trap.

| Where | Observed values |
|---|---|
| **Metric** `cost.usage` / `token.usage` | `main`, `auxiliary` (docs add `subagent`) — a **coarse 3-value category** |
| **Log event** `api_request` / `assistant_response` | `sdk`, `generate_session_title`, **`agent:builtin:general-purpose`** — a **fine-grained subsystem string** |

Same attribute name, disjoint value sets. Polis must parse them separately. The event-side value is
the more useful one: the `agent:builtin:<type>` form identifies subagent API traffic (§7b).

### 4.4 `model` is spelled differently on metrics vs events [OBSERVED — docs do not flag this]

In the same session, for the same request:

- `claude_code.api_request` event → `model=claude-opus-5`
- `claude_code.cost.usage` metric → `model=claude-opus-5[1m]`

The metric carries the **context-window-suffixed** model id; the event carries the base id. Joining
metrics to events on `model` requires normalising the `[1m]` suffix.

---

## 5. Log events

### 5.1 Complete inventory of event names

39 `claude_code.*` names appear in the reference. Excluding the 8 metric names (§4.2) and the 5 span
names (§6), the **event** set is:

`api_error`, `api_refusal`, `api_request`, `api_request_body`, `api_response_body`,
`api_retries_exhausted`, `assistant_response`, `at_mention`, `auth`, `compaction`,
`feedback_survey`, `hook_execution_complete`, `hook_execution_start`, `hook_plugin_metrics`,
`hook_registered`, `internal_error`, `mcp_server_connection`, `permission_mode_changed`,
`plugin_installed`, `plugin_loaded`, `retention_sweep`, `skill_activated`, `subagent_completed`,
`tool_decision`, `tool_result`, `user_prompt`.

**Observed on the wire in my captures (9):** `plugin_loaded`, `mcp_server_connection`,
`permission_mode_changed`, `user_prompt`, `api_request`, `assistant_response`, `tool_decision`,
`tool_result`, `subagent_completed`.

### 5.2 `claude_code.tool_result` — the event Polis lives on [OBSERVED]

Logged when a tool **completes**. **Not emitted for rejected calls** — a rejected call produces only
a `tool_decision`. Full observed record (a `Read`):

```
event.name=tool_result
event.timestamp=2026-09-01T17:38:50.290Z
event.sequence=12
prompt.id=09a624c0-ca45-4e72-aeef-0ce917a44d18
tool_name=Read
tool_use_id=toolu_01XZwEveLpHVQ361BFSPFwPV
success=true
duration_ms=3
tool_input={"file_path":"C:\\Users\\konka\\…\\probe2\\target.txt"}
tool_input_size_bytes=153
tool_result_size_bytes=54
```

| Attribute | Type | Notes |
|---|---|---|
| `tool_name` | string | `Read`, `Write`, `Edit`, `Agent`, `Bash`, `ToolSearch`, … For user-configured MCP servers this is the literal `mcp_tool`. |
| `tool_use_id` | string `toolu_…` | **Joins to the hook payload's `tool_use_id` and to the trace span.** |
| `success` | string `"true"`/`"false"` | Note: **string**, not bool. |
| `duration_ms` | int | Execution time. |
| `tool_input` | **JSON string** | Gated on `OTEL_LOG_TOOL_DETAILS=1`. Docs say values >512 chars are truncated; **observed limit is 128 chars** + a `…[N chars]` marker. Truncation is **per-value, so the JSON stays valid**. Whole payload capped ~4 K. |
| `tool_input_size_bytes` | int | Size of the JSON-serialized input. |
| `tool_result_size_bytes` | int | Size of the result. |
| `tool_parameters` | **JSON string** | Gated. Tool-specific digest — see below. |
| `error_type` | string | e.g. `Error:ENOENT`, `ShellError`. Only on failure. |
| `error` | string | Full message. Gated on `OTEL_LOG_TOOL_DETAILS=1`. Only on failure. |
| `decision_type` | string | Always `accept` (rejects produce no result). **Not observed** — may be version-gated. |
| `decision_source` | string | `config` \| `hook` \| `user_permanent` \| `user_temporary`. **Not observed.** |
| `mcp_server_scope` | string | MCP tools only. |

`tool_parameters` contents by tool **[DOCUMENTED]**: Bash → `bash_command`, `full_command`,
`timeout`, `description`, `dangerouslyDisableSandbox`, `git_commit_id` (SHA, when a `git commit`
succeeds — a free commit signal); MCP → `mcp_server_name`, `mcp_tool_name`; Skill → `skill_name`;
Agent/Task → `subagent_type` **[OBSERVED]**.

> **Docs/observation mismatch.** The reference lists `decision_type` and `decision_source` on
> `tool_result`; **neither appeared on any of my 9 captured `tool_result` records** at v2.1.248,
> with `OTEL_LOG_TOOL_DETAILS=1`. Observed wins: **treat both as optional.**

### 5.3 `claude_code.tool_decision` [OBSERVED]

Logged when a permission decision is made — **including rejections**, which is its whole value.

```
event.name=tool_decision  event.sequence=11  prompt.id=09a624c0-…
decision=accept  source=user_permanent  tool_name=Read
tool_use_id=toolu_01XZwEveLpHVQ361BFSPFwPV  tool_source=builtin
```

| Attribute | Type | Values |
|---|---|---|
| `tool_name` | string | as above |
| `tool_use_id` | string | joins to `tool_result` and to hooks |
| `decision` | string | `accept` \| `reject` |
| `source` | string | `config` \| `hook` \| `user_permanent` \| `user_temporary` \| `user_abort` \| `user_reject` **[OBSERVED `config`, `user_permanent`]** |
| `tool_source` | string | `builtin` \| `mcp` \| `sdk_host_builtin_mcp` **[OBSERVED `builtin`]** |
| `tool_parameters` | JSON string | Gated. **[OBSERVED `{"subagent_type":"general-purpose"}` on the Agent call]** |

**Critical for Polis: `tool_decision` carries NO `tool_input` and therefore NO file path.**
On my `Read`/`Write` decisions the only tool-specific attribute was `tool_parameters`, and it was
**absent entirely** for `Read` and `Write` (it appeared only for `Agent`). So the path of an
Edit/Write is **not knowable at decision time** from OTel — only after completion, from
`tool_result`. If Polis needs pre-execution path knowledge (e.g. to paint an intent before the
write lands), that must come from the `PreToolUse` **hook**, not from OTel.

Semantics worth noting **[DOCUMENTED]**: in `-p`/SDK sessions, `user_permanent` and `user_temporary`
are emitted for *every* matching call, whereas in the interactive CLI they are emitted only for the
prompt answer itself and later matches report `config`. My `-p` capture shows exactly this
(`source=user_permanent` on a plain `Read`).

### 5.4 `claude_code.user_prompt` [OBSERVED]

```
event.name=user_prompt  event.sequence=10  prompt.id=09a624c0-…
prompt_length=490  prompt=<REDACTED>  message.uuid=c8011bf6-…
```

| Attribute | Type | Notes |
|---|---|---|
| `prompt.id` | UUID | **The turn boundary.** A new `user_prompt` starts a new `prompt.id`. |
| `prompt_length` | int | Characters. Emitted even when the text is redacted — useful without leaking. |
| `prompt` | string | `<REDACTED>` unless `OTEL_LOG_USER_PROMPTS=1`. **[OBSERVED redacted by default]** |
| `message.uuid` | UUID | Matches the transcript JSONL entry. Absent for command dispatches. |
| `command_name` | string | When the prompt invokes a command. Custom/plugin/MCP names collapse to `custom`/`mcp` unless `OTEL_LOG_TOOL_DETAILS=1`. |

### 5.5 `claude_code.api_request` [OBSERVED]

```
event.name=api_request  event.sequence=8  prompt.id=…
model=claude-haiku-4-5-20251001  input_tokens=900  output_tokens=9
cache_read_tokens=0  cache_creation_tokens=0
cost_usd=0.000945  cost_usd_micros=945  duration_ms=890
request_id=req_011Ced6L5Q4irV6yUGmCTLvm
client_request_id=a34db923-…  speed=normal  query_source=generate_session_title
```

Plus `effort` (`low`|`medium`|`high`|`xhigh`|`max`) when the model supports it **[OBSERVED
`effort=xhigh`]**. `cost_usd_micros` is the integer form — **prefer it over the float `cost_usd`**
for exact accumulation.

`query_source` **[OBSERVED]**: `sdk`, `generate_session_title`, `agent:builtin:general-purpose`.
The last form is how a subagent's API traffic identifies itself on the logs channel.

### 5.6 `claude_code.assistant_response` [OBSERVED]

`response_length` (int), `response` (`<REDACTED>` by default), `model`, `request_id`,
`message.uuid`, `query_source`, `prompt.id`.

### 5.7 `claude_code.subagent_completed` [OBSERVED]

Logged when a subagent returns to its parent.

```
event.name=subagent_completed  event.sequence=23  prompt.id=…
agent_type=general-purpose  agent.source=built-in  is_built_in=true  is_async=false
total_tokens=17824  total_tool_uses=1  duration_ms=5117
model=claude-haiku-4-5-20251001  final_model=claude-haiku-4-5-20251001  model_swapped=false
```

| Attribute | Type | Notes |
|---|---|---|
| `agent_type` | string | Subagent type. Non-built-in names → `custom` unless `OTEL_LOG_TOOL_DETAILS=1`. |
| `agent.source` | string | `built-in` \| `plugin` \| `userSettings` \| `projectSettings` |
| `is_built_in` / `is_async` / `model_swapped` | string bool | `is_async=true` ⇒ background subagent — see §9. |
| `total_tokens` | int | **Trap:** only the subagent's *final* API request, **not a sum across the run**. |
| `total_tool_uses` | int | Tool calls across the whole run. This one *is* a total. |
| `duration_ms` | int | Wall clock. |
| `model` / `final_model` | string | Differ after a mid-run fallback. |

**No subagent identifier.** This event carries no id that links it to the individual tool calls the
subagent made. See §7(b).

### 5.8 `claude_code.mcp_server_connection` [OBSERVED]

`status` (`connected`|`disconnected`), `transport_type` (`stdio`, `claudeai-proxy`), `server_scope`
(`claudeai`, `dynamic`), `duration_ms`, `is_plugin`, `plugin_id_hash`, `plugin.name`, `server_name`
(gated), `error` (gated).

### 5.9 `claude_code.plugin_loaded` [OBSERVED]

`plugin.name`, `marketplace.name`, `plugin.version`, `plugin.scope`, `enabled_via`
(`default-enable`|`org-policy`|`admin-install`|`seed-mount`|`user-install`), `plugin_id_hash`,
`has_hooks`, `has_mcp`, `host_owned_mcp`, `skill_path_count`, `command_path_count`,
`agent_path_count`, `safe_mode`. Emitted at startup, **before any `prompt.id` exists**.

### 5.10 `claude_code.permission_mode_changed` [OBSERVED]

`from_mode`, `to_mode` (`default`|`plan`|`acceptEdits`|`auto`|`bypassPermissions`), `trigger`
(`shift_tab`|`exit_plan_mode`|`auto_gate_denied`|`auto_opt_in`; absent for SDK/bridge transitions).
**[OBSERVED `from_mode=auto to_mode=default trigger=auto_gate_denied`]** — and note it carried
**no `prompt.id`** because it fired before the first prompt.

### 5.11 Events Polis should know exist but not depend on [DOCUMENTED]

`api_error` (adds `error`, `status_code`, `attempt`), `api_refusal` (adds `category`:
`cyber`|`bio`|`frontier_llm`|`reasoning_extraction`, gated), `api_retries_exhausted`,
`skill_activated` (`skill.name`, → `custom_skill` unless gated), `at_mention`, `compaction`,
`auth`, `internal_error`, `plugin_installed`, `hook_registered`, `hook_execution_start`,
`hook_execution_complete`, `retention_sweep`, `feedback_survey`.
`api_request_body` / `api_response_body` appear **only** under `OTEL_LOG_RAW_API_BODIES` — Polis
must never enable it, and should **drop these two names on sight** if they ever arrive (§8).

---

## 6. Traces (beta) — required for Polis's thread model

Enabled by `CLAUDE_CODE_ENABLE_TELEMETRY=1` + `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1` +
`OTEL_TRACES_EXPORTER=otlp`. **Verified working on this machine at v2.1.248.**

### 6.1 Span hierarchy [OBSERVED — reconstructed from real parent ids]

```
claude_code.interaction                      (root; one per user prompt)
├── claude_code.llm_request                  (main-agent API calls)
└── claude_code.tool                         (one per tool call; carries file_path + agent_id)
    ├── claude_code.tool.blocked_on_user     (permission wait)
    └── claude_code.tool.execution           (actual execution)
        └── (Agent tool only) the subagent's own
            claude_code.llm_request and claude_code.tool spans,
            each stamped agent_id=<subagent id>
```

Observed instance: interaction `1b69803d0429f2e8` → Agent tool span `b344e6ce2b77ceea` →
its `tool.execution` `5cd450b9b4343a2f` → subagent's `claude_code.tool` `66006a7daf29b65b`
carrying `agent_id=a106e5fe86fce476a`.

**Auxiliary requests get their own trace.** The session-title `llm_request` arrived with
`parent=-` (root) under a **different `trace_id`** and `llm_request.context=standalone`. So
`trace_id` is *not* one-per-session; it is one-per-interaction-or-standalone-request.

### 6.2 Span attributes [OBSERVED]

Every span carries the §3 standard attributes plus `span.type` (equal to the span name minus the
`claude_code.` prefix).

**`claude_code.interaction`**: `user_prompt` (`<REDACTED>` by default), `user_prompt_length`,
`interaction.sequence`, `interaction.duration_ms`.

**`claude_code.llm_request`**: `model`, `gen_ai.system=anthropic`, `gen_ai.request.model`,
`llm_request.context` (`standalone` | `tool` **[OBSERVED both]**), `speed`, **`agent_id`**,
`duration_ms`, `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_creation_tokens`,
`success`, `attempt`, `request_id`, `gen_ai.response.id`, `client_request_id`, `ttft_ms`,
`stop_reason`, `gen_ai.response.finish_reasons` (array).

**`claude_code.tool`** — *the important one*:

| Attribute | Notes |
|---|---|
| `tool_name` | |
| **`file_path`** | **Target path for Read, Edit and Write — a first-class top-level attribute.** Gated on `OTEL_LOG_TOOL_DETAILS=1`. **[OBSERVED for both Read and Write]** |
| **`agent_id`** | **Identifier of the subagent that ran the tool. ABSENT on the main session.** **[OBSERVED]** |
| `parent_agent_id` | The spawning agent. Absent for the main session *and* for agents spawned directly from it. **[not observed — my subagent was spawned directly from main, exactly as documented]** |
| `tool_use_id`, `gen_ai.tool.call.id` | Same value; **joins the span to the `tool_result` / `tool_decision` log events and to hook payloads.** |
| `duration_ms`, `result_tokens` | |
| `full_command` | Bash command string. Gated. |
| `skill_name`, `subagent_type` | Gated. |
| `workflow.run_id`, `workflow.name` | Workflow runs only. |

**`claude_code.tool.execution`**: `tool_use_id`, `gen_ai.tool.call.id`, `duration_ms`, `success`.
**`claude_code.tool.blocked_on_user`**: `duration_ms`, `decision`, `source`
(**[OBSERVED both as the literal string `unknown`]** in a `-p` session — do not trust these two;
use the `tool_decision` event instead).
**`claude_code.hook`**: requires *detailed* beta tracing (`ENABLE_BETA_TRACING_DETAILED=1` +
`BETA_TRACING_ENDPOINT`), which **redirects logs and traces to that endpoint** and needs org
allowlisting for interactive sessions. **Polis must not enable detailed beta tracing** — it would
hijack the export destination.

### 6.3 Enabling traces retro-fits correlation ids onto the LOG records [OBSERVED]

This is a significant, easily-missed benefit.

| Traces enabled? | `trace_id` / `span_id` on log records |
|---|---|
| No (`OTEL_LOGS_EXPORTER` only) | **empty on all 32 records** |
| Yes | **populated on all interaction-scoped records** |

With tracing on, each log record is stamped with the enclosing span:

| Log event | Stamped `span_id` | Meaning |
|---|---|---|
| `user_prompt`, `api_request`, `assistant_response`, `tool_result` (main agent) | the **`interaction`** span | main-agent work |
| `api_request`, `assistant_response`, `tool_result`, `subagent_completed` (subagent) | the Agent call's **`tool.execution`** span | subagent work |
| `tool_decision` | its own **`tool.blocked_on_user`** span | parent-of-parent is the `claude_code.tool` span |
| `plugin_loaded`, `mcp_server_connection`, `permission_mode_changed` | empty | emitted outside any interaction |

So `span_id` alone separates main-agent from subagent `tool_result`s **without parsing the trace
tree** — see §7(b).

---

## 7. Signal matrix: what Polis needs, and whether OTel provides it

### (a) The file path of an Edit / Write / Read — **YES**, two routes

**Route 1 (logs, no beta flags): `claude_code.tool_result` → `tool_input`.**
The path is **not** a top-level attribute. It is inside the **JSON string** attribute `tool_input`:

```
tool_input={"file_path":"C:\\Users\\…\\probe2\\target.txt"}
```

Requires `OTEL_LOG_TOOL_DETAILS=1`. Absolute, OS-native (backslashes on Windows). Polis must
`serde_json::from_str` the attribute and read `.file_path`. Confirmed for `Read` (with an extra
`"limit":1` key) and `Write` (with `"content":"DONE"` — note the **file body rides along**, §8).
This is what the sibling agent's `rx.log` was surfacing.

**Route 2 (traces, needs the beta pair): `claude_code.tool` span → `file_path`.**
A first-class top-level string attribute, no JSON parsing, also gated on `OTEL_LOG_TOOL_DETAILS=1`:

```
SPAN name=claude_code.tool … tool_name=Read file_path=C:\Users\…\probe2\target.txt
```

**Recommendation:** prefer Route 2 (cleaner, and it is the same span that carries `agent_id`), fall
back to Route 1. Both are keyed by `tool_use_id`, so they merge cleanly.

**`Edit` verified specifically [OBSERVED]** — this was the case most likely to break Route 1, so I
probed it with a 713-character `old_string`. Both routes hold:

- The `claude_code.tool` span carried `tool_name=Edit` with a clean top-level
  `file_path=…\probe2\big.py`.
- The `tool_result` `tool_input` was:

  ```json
  {"file_path":"C:\\…\\big.py",
   "old_string":"    return \"XXXX…[713 chars]",
   "new_string":"    return \"SHORT\"",
   "replace_all":false}
  ```

Three properties that make Route 1 safe, all verified mechanically:

1. **Truncation is per-value, not a blind cut of the payload**, so **the JSON remains valid** —
   `json.loads` succeeded on the captured string and yielded all four keys.
2. **`file_path` is the first key** and, being short, is never the value that gets truncated.
3. The truncation marker is `…[N chars]` (U+2026 + the original length) appended **inside** the
   string value.

> **Docs/observation mismatch.** The reference says "individual values over **512** characters are
> truncated". Observed at v2.1.248: a 713-char `old_string` was cut to **128 characters** plus the
> marker (140 total). The real per-value budget is far tighter than documented. It does not threaten
> `file_path`, but **never assume any other `tool_input` value is complete** — always check for the
> `…[` marker before treating a value as whole.

### (b) Distinguishing a SUBAGENT's tool call from a MAIN AGENT's

**On the logs channel alone: NO. Definitively not.** This is the critical architectural finding.

I ran a session where the main agent read `target.txt` and a `general-purpose` subagent read the
*same* file. The two `tool_result` records are **structurally identical**:

```
seq=12  tool_name=Read tool_use_id=toolu_01XZw… success=true duration_ms=3 tool_input={…target.txt}   <- MAIN
seq=19  tool_name=Read tool_use_id=toolu_01WtV… success=true duration_ms=1 tool_input={…target.txt}   <- SUBAGENT
```

Same `session.id`, same `prompt.id`, no agent field. Verified mechanically: **`agent_id` appears 0
times** and **`query_source` appears 0 times** across every `tool_result` and `tool_decision` in the
capture. `subagent_completed` gives only aggregates (`total_tool_uses=1`) with no per-call linkage.

The only logs-only fallback is **temporal nesting**: the subagent's calls fall between the Agent
tool's `tool_decision` (`event.sequence=15`) and its `tool_result` (`event.sequence=24`). That
inference **breaks completely** for background subagents (`is_async=true`) and for concurrent
subagents, where several windows overlap and the main agent keeps working inside them. **Do not
build the thread model on it.**

**With the traces channel enabled: YES, cleanly, by two independent means.**

1. **`agent_id` on the `claude_code.tool` span.** Observed exactly as documented — present on the
   subagent's Read (`agent_id=a106e5fe86fce476a`), **absent** on the main agent's. Absence *is* the
   main-agent signal. `parent_agent_id` additionally identifies the spawner for nested subagents.
2. **`span_id` on the `tool_result` log record** (§6.3). Main-agent results are stamped with the
   `interaction` span; the subagent's is stamped with the Agent call's `tool.execution` span.
   Concurrent subagents get distinct `tool.execution` spans, so this stays unambiguous.

**Consequence for the PRD:** Polis's thread model is only viable if the injected environment enables
the beta traces channel. §4.1's env block must gain `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1` and
`OTEL_TRACES_EXPORTER=otlp`, and the receiver must implement `TraceService` alongside
`LogsService`/`MetricsService`. Because the channel is beta, Polis must also **degrade gracefully**:
if no spans arrive, fall back to "all tool calls belong to the main agent" rather than mis-attributing.

### (c) The correlation key joining events to a session/turn

**The PRD is correct: the attribute really is named `prompt.id`** (dotted, not `prompt_id`).
**[OBSERVED]** UUID v4, e.g. `09a624c0-ca45-4e72-aeef-0ce917a44d18`.

Two-level key:

- **`session.id`** — the session. On **every** event and **every** metric datapoint.
- **`prompt.id`** — the turn. On **events only**; deliberately never on metrics (cardinality).
  It links every event produced while processing one user prompt, **including the subagent's**.

Measured `prompt.id` coverage over the whole capture:

| Event | Carries `prompt.id`? |
|---|---|
| `user_prompt`, `tool_decision`, `tool_result`, `subagent_completed` | **always** (4/4, 4/4, 1/1) |
| `api_request`, `assistant_response` | 6/7 and 5/6 — **missing on the pre-prompt startup request** |
| `mcp_server_connection` | 4/8 — only after the first prompt |
| `plugin_loaded`, `permission_mode_changed` | **never** (startup, before any prompt) |

**So `prompt.id` is optional and Polis must handle its absence** — startup-phase events legitimately
have none. Do not make it a required field in the parser.

Supporting join keys:

| Key | Joins |
|---|---|
| `tool_use_id` (`toolu_…`) | `tool_decision` ↔ `tool_result` ↔ `claude_code.tool` span ↔ **hook payloads** (Channel B). The strongest cross-channel key. |
| `event.sequence` | Total order within a session, from 0. Use for ordering, **not** `event.timestamp`. |
| `request_id` (`req_…`) | `api_request` ↔ `assistant_response` ↔ transcript `requestId`. |
| `client_request_id` | Pairs a request with its response even when it failed before the server assigned a `request_id`. First-party API only. |
| `message.uuid` | `user_prompt`/`assistant_response` ↔ transcript JSONL entries (Channel D). Docs warn the transcript format is internal and version-specific. |
| `trace_id` / `span_id` | Only when tracing is on (§6.3). |

---

## 8. Sensitive data — what Polis must never persist

The default stream is **already not clean**. Observed in plaintext with only
`OTEL_LOG_TOOL_DETAILS=1` set:

| Attribute | Observed | Why it matters |
|---|---|---|
| `user.email` | `operator@example.com` | Real PII, on **every event and every metric datapoint**. No env var suppresses it. |
| `user.account_uuid`, `user.account_id` | real ids | Suppress with `OTEL_METRICS_INCLUDE_ACCOUNT_UUID=false`. |
| `organization.id` | real UUID | Always included when available. |
| `user.id` | 64-hex | Anonymous per the docs, but a stable machine fingerprint. |
| **`tool_input` on a `Write`** | `{"file_path":…,"content":"DONE"}` | **The file's contents are in the attribute.** For a real Write this is source code. |
| `tool_input` on an `Edit` | (not captured) | Will contain `old_string`/`new_string` — i.e. code diffs. |
| `full_command` in `tool_parameters` | Bash commands | Can contain secrets typed into a command line. |
| `server_name`, `plugin.name` | third-party names | Docs note these are deliberately redacted by default *from Anthropic*, but flow to your backend once gated on. |

**Rules for Polis:**

1. **Never set** `OTEL_LOG_USER_PROMPTS`, `OTEL_LOG_ASSISTANT_RESPONSES`, `OTEL_LOG_TOOL_CONTENT`,
   or `OTEL_LOG_RAW_API_BODIES`. Verify prompts arrive as `<REDACTED>` and treat a non-redacted
   `prompt`/`response` as a misconfiguration to warn about.
2. **Set `OTEL_METRICS_INCLUDE_ACCOUNT_UUID=false`.**
3. **Extract, then discard.** From `tool_input`, pull `file_path` and drop the rest of the JSON
   before anything reaches disk. Never persist the raw `tool_input`/`tool_parameters` strings.
4. **Drop `user.email` at ingest.** Polis needs a stable agent identity, not an identity document;
   `session.id` and `user.id` suffice. If a display name is wanted, hash the email.
5. **Refuse `api_request_body` / `api_response_body`** by name at the parser boundary — they carry
   entire conversation histories, and their presence means someone set `OTEL_LOG_RAW_API_BODIES`.
6. Port 4317 on loopback is **unauthenticated**. Any local process can post arbitrary OTLP. Check
   `service.name == "claude-code"` and treat all payloads as untrusted input (§9).

---

## 9. Defensive parsing notes

**Unknown event names.** The event name is the record **body** string, not `event_name`. Strip the
`claude_code.` prefix, match, and on no match log at debug and **drop**. The docs list 26 event
names; my captures produced 9. New names ship with new CLI versions — an unknown name is normal
traffic, never an error. Never `panic!`/`unwrap` on it.

**Missing attributes are the norm, not the exception.** Concretely verified:
`prompt.id` is absent on startup events; `decision_type`/`decision_source` are documented on
`tool_result` but were never emitted; `tool_parameters` is absent for `Read`/`Write` but present for
`Agent`; `agent_id` is absent precisely when the actor is the main agent; `effort` is absent for
models that don't support it. **Every attribute must be `Option`.** Absence is frequently
*semantically meaningful* (no `agent_id` ⇒ main agent) rather than a parse failure.

**Types on the wire are not the types in the docs.** Booleans arrive as the **strings** `"true"` /
`"false"` (`success`, `is_built_in`, `is_async`, `model_swapped`). Numbers may arrive as OTLP
`IntValue` **or** `DoubleValue` depending on the value — `duration_ms` came through as a double
(`dur=Some(2.0)`). Accept both and coerce; never match on a single `AnyValue` variant.

**Spans arrive before their parents.** OTLP batches are **not topologically ordered** — in my
capture `tool.blocked_on_user` (`5dd3542d…`) arrived before its parent `claude_code.tool`
(`1a85aeac…`), and `claude_code.tool` arrived before the `interaction` root. Polis must buffer spans
by `span_id` and resolve parentage lazily, never assume a parent already exists.

**`trace_id` is not per-session.** Auxiliary requests form their own root traces
(`llm_request.context=standalone`). Do not use `trace_id` as a session key; use `session.id`.

**Delta counters.** Accumulate, don't assign — and read `aggregation_temporality` off the wire
rather than assuming (§4.1).

**Order by `event.sequence`, not timestamps.** `event.timestamp` is ms-resolution and I observed
**ties** (three `plugin_loaded` events sharing `…16.190Z`, and eight metric datapoints sharing one
`time_unix_nano`). `event.sequence` is a monotonic per-session integer from 0 — the only reliable
total order. It also detects **loss**: a gap means records were dropped.

**Two `query_source` vocabularies and two `model` spellings.** §4.3 and §4.4. Don't join naively.

**Schema drift.** Pin observations to `service.version` / the logs scope version (both `2.1.248`
here) and log once when an unseen version appears. The reference is explicit that attributes were
added and semantics *changed* across v2.1.191→v2.1.251; assume the same rate going forward. Prefer
"parse what I recognise, retain the rest as opaque key/value pairs" over a strict struct.

**Adversarial input.** Bound everything: `max_decoding_message_size` (16 MiB used here), a cap on
attributes per record, and a cap on `tool_input` JSON length before parsing. Cap the ingest channel
and **drop on full** — the receiver must never block, because backpressure would reach the agent.
My probe uses `crossbeam` `try_send` with a dropped-counter for exactly this.

**Degrade, don't die.** Bind the socket **eagerly on the calling thread** so `AddrInUse` (WSAEADDRINUSE
10048 on Windows) surfaces as a matchable `io::Error` rather than a panic inside a Tokio runtime on a
background thread. On failure, warn and run without Channel A. Verified working (`server.err`).

---

## 10. PRD corrections

| # | PRD claim (§4.1) | Reality | Recommended change |
|---|---|---|---|
| 1 | Env block lists 8 vars and no tracing | Without `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1` + `OTEL_TRACES_EXPORTER=otlp`, **a subagent's tool call cannot be distinguished from the main agent's** (§7b) | Add both vars + `OTEL_TRACES_EXPORT_INTERVAL=1000`. Add `TraceService` to the receiver. Flag the thread model as depending on a **beta** signal, with a defined fallback |
| 2 | "Polis embeds an OTLP/gRPC receiver on 4317 (`tonic` + `opentelemetry-proto`)" | Correct, and verified end-to-end. But `tonic` 0.14 needs **`tonic-prost`** as a separate crate, and `opentelemetry-proto` needs the **`trace`** feature | Record the exact dependency block (§11) |
| 3 | "**VERIFY:** … the `prompt.id` correlation attribute" | **Confirmed.** The attribute is literally `prompt.id` | Change VERIFY to a statement; add that it is **absent on startup events** and never on metrics |
| 4 | Implies tool file paths are directly available | On the logs channel the path is **nested inside the `tool_input` JSON string**, not a top-level attribute. A top-level `file_path` exists only on the **`claude_code.tool` span** | State both routes; prefer the span |
| 5 | `OTEL_METRIC_EXPORT_INTERVAL=10000` and `OTEL_LOGS_EXPORT_INTERVAL=1000` presented as settings | Correct, but the **defaults** are 60000 and 5000 — the PRD's values are a deliberate 6×/5× speed-up | Note the defaults so the override is understood as intentional |
| 6 | Nothing about counter temporality | All counters are **DELTA** — increments since last export, not running totals. Consuming them as cumulative silently under-reports everything | Add: accumulate deltas; read `aggregation_temporality` off the wire |
| 7 | Nothing about PII | `user.email` rides on **every event and metric datapoint** by default, and a `Write`'s `tool_input` contains **the file's contents** | Add the §8 rules; set `OTEL_METRICS_INCLUDE_ACCOUNT_UUID=false`; extract-then-discard `tool_input` |
| 8 | "unknown event types are logged at debug and dropped, never fatal" | Correct and necessary — but insufficient. Missing *attributes* on **known** events are equally routine, and absence is often meaningful | Extend the rule to attributes: every field `Option`, absence never fatal |
| 9 | §4.2 relies on hooks for `PermissionRequest`-style latency events | `tool_decision` gives the **same decision data over OTel**, including rejects, with no process spawn — but it is emitted *after* the fact and carries **no file path** | Keep the hook for pre-execution/latency-critical paths; use `tool_decision` for the audit record |

---

## 11. Reference code

### 11.1 Receiver dependencies — exact versions compiled and run on this machine

```toml
[dependencies]
tonic = { version = "0.14.6", default-features = false, features = ["router", "server", "transport", "codegen"] }
tonic-prost = "0.14.6"                 # REQUIRED: tonic 0.14 split prost codegen into its own crate
prost = "0.14.4"
opentelemetry-proto = { version = "0.32.0", default-features = false, features = [
    "gen-tonic", "logs", "metrics", "trace",   # "trace" is REQUIRED for subagent attribution
] }
tokio = { version = "1.53.1", features = ["rt-multi-thread", "net", "macros", "signal", "time", "sync"] }
tokio-stream = { version = "0.1.17", features = ["net"] }
crossbeam-channel = "0.5.16"
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
```

The receiver registers **three** services: `LogsServiceServer`, `MetricsServiceServer`,
`TraceServiceServer`. Omitting the third makes Claude Code's trace exports fail with `UNIMPLEMENTED`
and costs Polis its thread model.

Two patterns proven in the probe and worth carrying into Polis:

- **Bind eagerly on the calling thread** with `std::net::TcpListener::bind`, match
  `ErrorKind::AddrInUse`, then hand the listener to Tokio via `TcpListener::from_std`. This turns
  port contention into a warning instead of a panic on a background runtime thread.
- **`try_send` into a bounded `crossbeam` channel** with a dropped-counter. The gRPC handler must
  never block, or backpressure reaches the agent.

### 11.2 Attribute-key constants

The full `polis-otel-keys` module — every constant below plus `Temporality::accumulate`,
`file_path_from_tool_input`, `is_truncated`, `parse_wire_bool` and `actor_from_span_attrs` — is in
the `reference_code` field of this task's structured output. It compiles on rustc 1.94.0
(`x86_64-pc-windows-msvc`) and ships **8 unit tests that pass against strings captured verbatim from
the wire**, including the truncated `Edit` payload and the delta-vs-cumulative accumulation
difference.

---

## 12. Open items not resolved here

1. ~~`Edit` was never exercised.~~ **Resolved** — probed with a 713-char `old_string`; see §7(a).
   Tools exercised across all captures: `Read`, `Write`, `Edit`, `Agent`, `ToolSearch`.
   Still unexercised: `Bash`/`PowerShell`, `NotebookEdit`, MCP tools, `Skill`.
2. **Concurrent / background subagents** (`is_async=true`) were not exercised. The claim that
   distinct `tool.execution` spans keep concurrent subagents unambiguous is **reasoned from the span
   tree, not observed**. Verify before building on it.
3. **`parent_agent_id`** was never populated (my subagent was spawned directly from main, where the
   docs say it is absent). Nested-subagent attribution is documented but unverified.
4. **Rejected tool calls** were not exercised — `decision=reject` and the `user_abort`/`user_reject`
   sources are documented only.
5. **`claude_code.hook` spans** require detailed beta tracing, which redirects the export
   destination. Deliberately not tested; Polis should not enable it.
6. **Bash/`full_command`** shape unverified on Windows, where the shell tool may be `PowerShell`
   rather than `Bash` (see `hooks-schema.md`).
