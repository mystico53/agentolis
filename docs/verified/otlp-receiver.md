# Verified: OTLP/gRPC receiver (Channel A, PRD §4.1)

**Status:** verified 2026-09-01 by compiling and running a receiver, then pointing a **live Claude Code 2.1.248 session** at it.
**Primary source:** live wire capture (two real sessions + one zero-cost session).
**Secondary source:** `https://code.claude.com/docs/en/monitoring-usage` (raw markdown, 139,812 bytes, local copy `otel.md`).
**Crate sources read:** `opentelemetry-proto 0.32.0`, `tonic 0.14.6`, `tonic-prost 0.14.6` from `~/.cargo/registry/src/index.crates.io-*/`.
**Toolchain:** rustc/cargo 1.94.0, `x86_64-pc-windows-msvc`, Windows 11 Pro 26200.

Everything here was either measured on this machine or read out of the crate source. Where the PRD conflicts,
this file wins — see [§8 PRD corrections](#8-prd-corrections).

**Scope, and how this relates to `otel-schema.md`.** That companion doc is the authority on the *schema*:
the env-var surface, the full event/attribute inventory, the beta trace spans, and the signal matrix. **This
doc is the authority on the *receiver*:** which crates and features to depend on, the exact generated module
paths, how to decode every value shape, and how to run the server inside a UI process without ever pushing
backpressure at an agent. Where the two overlap (temporality, resource block, `tool_input`) they were derived
independently from separate captures and **agree**; §2.4 folds in one of their observations.

Companion doc: `hooks-schema.md` (Channel B). This file assumes its findings.

---

## 1. Exact crate and feature set

`tonic` 0.14 **split prost codegen out into a separate `tonic-prost` crate**. The generated OTLP stubs call
`tonic_prost::ProstCodec::default()` — but that call lives *inside* `opentelemetry-proto`, which carries its own
`tonic-prost` dependency. **A receiver does not need `tonic-prost`, `prost`, or `prost-types` as direct
dependencies.** This was verified by building with them removed.

### 1.1 The dependency block (compile-verified, zero warnings)

```toml
[dependencies]
# Server only. `default-features = false` drops nothing we need; `gzip` is
# cheap insurance in case OTEL_EXPORTER_OTLP_COMPRESSION is ever set.
tonic = { version = "0.14.6", default-features = false, features = [
    "router", "server", "transport", "codegen", "gzip",
] }

# `gen-tonic` is what unlocks the *_service_server modules. `logs` and
# `metrics` gate the message modules themselves (see §1.2).
opentelemetry-proto = { version = "0.32.0", default-features = false, features = [
    "gen-tonic", "logs", "metrics",
] }

# No "macros": the receiver builds its runtime explicitly (§6.1), so there is
# no #[tokio::main] anywhere in Polis. "sync" is for the shutdown oneshot.
tokio = { version = "1.53.1", default-features = false, features = [
    "rt-multi-thread", "net", "time", "sync",
] }

# TcpListenerStream, required by serve_with_incoming_shutdown.
tokio-stream = { version = "0.1.17", default-features = false, features = ["net"] }

crossbeam-channel = "0.5.16"
serde_json = "1.0.151"   # tool_input is a JSON *string* — see §4.3
```

Resolved graph: **84 crates**, cold build 16.1 s, warm rebuild 2.2 s on 24 cores (measured after `cargo clean`).

### 1.2 Feature gating, read from the crate source

From `opentelemetry-proto-0.32.0/Cargo.toml`:

```
gen-tonic          = ["gen-tonic-messages", "tonic", "tonic-prost", "tonic/channel"]
gen-tonic-messages = ["prost"]
logs               = ["opentelemetry/logs",    "opentelemetry_sdk/logs"]
metrics            = ["opentelemetry/metrics", "opentelemetry_sdk/metrics"]
```

From `opentelemetry-proto-0.32.0/src/proto.rs`, the module tree is gated like this:

- `pub mod tonic` — `#[cfg(feature = "gen-tonic-messages")]`
- `tonic::collector::logs::v1` — `#[cfg(feature = "logs")]`
- `tonic::collector::metrics::v1` — `#[cfg(feature = "metrics")]`
- `tonic::common::v1`, `tonic::resource::v1` — **ungated**
- inside each generated file, `*_service_client` / `*_service_server` are `#[cfg(feature = "gen-tonic")]`

Two consequences worth knowing before someone tries to slim this down:

1. **You cannot get the collector service stubs without also pulling `opentelemetry` + `opentelemetry_sdk`.**
   The `logs`/`metrics` features are what expose the modules, and they transitively enable SDK features. Polis
   is a *receiver* and needs none of the SDK. This is ~2 unnecessary crates; accept it, or hand-roll a
   `tonic-prost-build` step against the raw `.proto` files if binary size ever matters.
2. **`gen-tonic` force-enables `tonic/channel`**, which drags in the hyper *client* stack (`hyper-util`
   client-legacy, `hyper-timeout`, `tower` balance/buffer/discover). Unavoidable for the same reason. It is
   also what makes the in-tree test client possible without extra deps.

### 1.3 Exact module paths of the generated server traits

These are the three paths, confirmed against the generated sources
(`src/proto/tonic/opentelemetry.proto.collector.<signal>.v1.rs`):

| Signal | Trait | Server type | gRPC service name |
|---|---|---|---|
| Logs | `opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsService` | `…::logs_service_server::LogsServiceServer<T>` | `opentelemetry.proto.collector.logs.v1.LogsService` |
| Metrics | `opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_server::MetricsService` | `…::metrics_service_server::MetricsServiceServer<T>` | `opentelemetry.proto.collector.metrics.v1.MetricsService` |
| Traces | `opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::TraceService` | `…::trace_service_server::TraceServiceServer<T>` | `opentelemetry.proto.collector.trace.v1.TraceService` |

Every trait has exactly one method, and it is `async fn export`:

```rust
#[async_trait]
pub trait LogsService: std::marker::Send + std::marker::Sync + 'static {
    async fn export(
        &self,
        request: tonic::Request<super::ExportLogsServiceRequest>,
    ) -> std::result::Result<tonic::Response<super::ExportLogsServiceResponse>, tonic::Status>;
}
```

Implement it with `#[tonic::async_trait]` (requires the `codegen` feature). The generated `…Server<T>` builder
exposes `new`, `from_arc`, `with_interceptor`, `accept_compressed`, `send_compressed`,
`max_decoding_message_size`, `max_encoding_message_size`.

**Polis registers Logs and Metrics only.** `TraceService` is dead weight unless detailed beta tracing
(`BETA_TRACING_ENDPOINT`) is ever turned on; drop the `trace` feature entirely. If a client ever *does* send
traces to an unregistered service, tonic answers `Unimplemented` and the exporter logs a `[3P telemetry]`
error — noisy but harmless. (Observed exactly this while an older probe that registered only Logs was still
holding the port.)

---

## 2. Live capture: what Claude Code 2.1.248 actually puts on the wire

Three captures, all on this machine:

| Capture | Session | Cost | Yield |
|---|---|---|---|
| A | full interactive session, default model | prior run | `tool_decision`, `tool_result` with a real absolute `file_path` |
| B | `claude -p "Reply with exactly: ok" --model haiku` | one trivial call | 17 log records, 12 metric points |
| C | same, but `ANTHROPIC_BASE_URL=http://127.0.0.1:9` | **zero** (connection refused) | `session.count` metric, scope version |

Env used (exactly PRD §4.1's block, plus `OTEL_METRIC_EXPORT_INTERVAL=3000` to shorten the wait):

```
CLAUDE_CODE_ENABLE_TELEMETRY=1
OTEL_LOGS_EXPORTER=otlp
OTEL_METRICS_EXPORTER=otlp
OTEL_EXPORTER_OTLP_PROTOCOL=grpc
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317
OTEL_LOGS_EXPORT_INTERVAL=1000
OTEL_METRIC_EXPORT_INTERVAL=3000
OTEL_LOG_TOOL_DETAILS=1
```

The PRD's block works verbatim. No auth headers, no TLS — plaintext h2c on loopback.

### 2.1 Resource attributes — exactly five, and **no session identity**

```
service.name=claude-code  service.version=2.1.248
os.type=windows  os.version=10.0.26200  host.arch=amd64
```

**This is the single most important structural fact for Polis.** Almost every OTLP consumer groups by
resource. That does not work here: **`session.id` is not a resource attribute.** It is stamped on every
*record* and every *data point* instead. One `ExportLogsServiceRequest` from one process carries exactly one
resource, but Polis must key agents off `attrs["session.id"]`, not off the resource block.

(`wsl.version` appears only under WSL. `service.name` becomes `claude-code-desktop` for Claude Desktop Code-tab
sessions — filter on both.)

### 2.2 Instrumentation scopes differ between signals

| Signal | Scope name | Scope version |
|---|---|---|
| Logs / events | `com.anthropic.claude_code.events` | `2.1.248` |
| Metrics | `com.anthropic.claude_code` | `2.1.248` |

The docs name only the meter (`com.anthropic.claude_code`). **The logs scope name is undocumented** and has a
`.events` suffix. Both carry the CLI version as the scope version, which tracks `service.version`.

Recommendation: **do not dispatch on scope.** Use `service.name` to recognize Claude Code and the record's own
`event.name` / body to recognize the event. Scope names are undocumented for logs and free to move.

### 2.3 LogRecord shape

Observed on every one of the 17 records:

| Field | Value | Note |
|---|---|---|
| `time_unix_nano` | set, ms precision (`…189000000`) | millisecond clock, nanos are always zero |
| `observed_time_unix_nano` | set | |
| `severity_number` | **`0` (UNSPECIFIED)** | never populated |
| `severity_text` | **`""`** | never populated |
| `event_name` (OTLP 1.7 field 12) | **empty** | Claude Code does not use it |
| `body` | `AnyValue::StringValue("claude_code.<event>")` | **fully-qualified** name |
| `attributes` | flat `KeyValue` list | see §2.4 |
| `trace_id` / `span_id` | **empty** | populated only under beta tracing |

Three traps:

1. **Never filter on severity.** Everything is `SEVERITY_NUMBER_UNSPECIFIED`. A receiver that drops
   non-INFO records drops everything.
2. **The event name lives in two places with two different spellings.** The `body` is
   `claude_code.tool_result`; the `event.name` *attribute* is the short `tool_result`. The docs describe both
   correctly, in different sections, which makes it easy to write a matcher against the wrong one. Polis should
   read `body` (it is always present) and treat `event.name` as a cross-check.
3. **`event_name`, the actual OTLP field for this, is empty.** Do not reach for it.

### 2.4 Attribute encoding

Standard attributes on every record (matches the documented "Standard attributes" table exactly):

```
session.id  organization.id  user.id  user.email  user.account_uuid  user.account_id  terminal.type
```

Plus per-event: `event.name`, `event.timestamp`, `event.sequence`, and `prompt.id` once a prompt exists.

Four encoding facts that decide how the decoder must be written:

- **`event.timestamp` is an ISO-8601 *string*** (`2026-09-01T17:40:36.074Z`), not an integer. Use
  `time_unix_nano` for ordering and treat this as display-only.
- **`event.sequence` is an integer, monotonic per session, starting at 0.** Undocumented in the PRD and
  extremely useful: it is a free gap detector. Polis should track the highest sequence per `session.id` and
  surface a hole as dropped telemetry rather than as a missing agent action.
- **Values contain spaces and colons.** `server_name=claude.ai Google Calendar` is one attribute value.
  Anything that reconstructs a log line by splitting on whitespace is broken.

#### The wire types are genuinely inconsistent — measured, not assumed

A variant dump over a live session (printing the actual `AnyValue` oneof arm per key) gives this:

```
user.id:string  session.id:string  organization.id:string  user.email:string
user.account_uuid:string  user.account_id:string  terminal.type:string
event.name:string  event.timestamp:string  event.sequence:int
--- plugin_loaded ---
plugin.name:string  plugin_id_hash:string  enabled_via:string
has_hooks:bool  has_mcp:bool  host_owned_mcp:bool          <- real booleans
safe_mode:string                                            <- string "false"!
skill_path_count:int  command_path_count:int  agent_path_count:int
--- mcp_server_connection ---
status:string  transport_type:string  server_scope:string
duration_ms:string                                          <- STRING, not a number
is_plugin:bool  server_name:string
--- user_prompt ---
prompt.id:string  prompt_length:string                      <- STRING, not a number
prompt:string  message.uuid:string
```

Three things fall out of this, and none of them are guessable:

1. **`duration_ms` is a `string_value`** on `mcp_server_connection`. So is `prompt_length`. A decoder that
   matches `IntValue`/`DoubleValue` for these keys gets `None` and silently loses the field.
   **The same key is a different type on a different event:** `otel-schema.md` records `duration_ms` as an
   `int` on `claude_code.tool_result`, captured independently. So the type is not even stable per *key* — it
   varies per *(event, key)* pair. This is the strongest possible argument for coercing rather than matching.
2. **Booleans are encoded two different ways inside the same event.** In `plugin_loaded`, `has_hooks`,
   `has_mcp` and `host_owned_mcp` are real `bool_value`s, while `safe_mode` is the *string* `"false"`. There is
   no rule to infer here; it has to be handled.
3. **Counts are real `int_value`s** (`event.sequence`, `*_path_count`), so the schema is not uniformly
   stringly-typed either.

The only safe rule: **coerce by key, accepting any scalar variant.** Read a numeric attribute as
"int, or double, or a string that parses as a number", and a boolean as "bool, or the strings `true`/`false`".
The reference decoder renders every variant to `String` first for exactly this reason.

(`success` on `tool_result` is documented as `"true"`/`"false"` — i.e. a string — consistent with `safe_mode`.
Not independently confirmed on the wire; no tool ran in the captured sessions.)

`user.email` is present and is real PII. Polis must not persist it by default.

### 2.5 Events observed live

`plugin_loaded`, `permission_mode_changed`, `mcp_server_connection`, `api_request`, `assistant_response`,
`user_prompt` (capture B/C); `tool_decision`, `tool_result` (capture A). `otel.md` carries **40** distinct
`claude_code.*` identifiers, but that set mixes three namespaces — log/event names, metric names, and beta
*trace span* names (`claude_code.llm_request`, `claude_code.tool.execution`, `claude_code.hook`). Only the
event names appear on the logs channel. Nothing was observed that is not documented.

`prompt.id` correlation — the PRD §4.1 **VERIFY** item — **is confirmed**. In capture B, `prompt.id`
`1baff33e-…` appears on the `user_prompt` that created it and then on every subsequent `api_request`,
`assistant_response`, and `mcp_server_connection` in that turn. Events emitted *before* the first prompt
(startup: `plugin_loaded`, initial `mcp_server_connection`, the session-title `api_request`) carry no
`prompt.id`. So `prompt.id` is a turn key, not a session key, and it is absent for startup traffic.

Content is redacted by default: `prompt=<REDACTED>` and `response=<REDACTED>`, with `prompt_length` /
`response_length` still present. `OTEL_LOG_USER_PROMPTS=1` / `OTEL_LOG_ASSISTANT_RESPONSES=1` unredact them.
**Polis should leave both off.**

---

## 3. AnyValue: every variant, handled

`AnyValue` is a oneof with 7 populated variants plus the absent case. `opentelemetry-proto` 0.32 exposes:

| Variant | Rust | Seen from Claude Code? |
|---|---|---|
| `string_value` | `Value::StringValue(String)` | yes, dominant — incl. `duration_ms`, `prompt_length`, `safe_mode` |
| `bool_value` | `Value::BoolValue(bool)` | yes (`has_hooks`, `has_mcp`, `is_plugin`) |
| `int_value` | `Value::IntValue(i64)` | yes (`event.sequence`, `*_path_count`) |
| `double_value` | `Value::DoubleValue(f64)` | not confirmed on the wire (see §2.4) |
| `array_value` | `Value::ArrayValue(ArrayValue)` | not yet (docs: `workspace.host_paths`) |
| `kvlist_value` | `Value::KvlistValue(KeyValueList)` | not yet |
| `bytes_value` | `Value::BytesValue(Vec<u8>)` | not yet |
| `string_value_strindex` | `Value::StringValueStrindex(i32)` | profiling-signal only; must not be fatal |
| *(absent)* | `value: None` | legal OTLP; must not panic |

The eighth arm, `StringValueStrindex` (tag 8), is new in the profiling protos. It will never appear in Logs or
Metrics, but the match must be exhaustive and non-fatal — that is a compile error waiting to happen on the next
`opentelemetry-proto` bump if you write `_ => unreachable!()`.

Two more non-obvious cases proven in the E2E: a `KeyValue` whose `value` is `Some(AnyValue { value: None })`
(an *empty* AnyValue) and one whose `value` is `None` entirely. Both are legal and both render as empty string.
`f64::NAN` renders as `NaN`, not `null`.

The reference `flatten_attributes` (§9) flattens nested `kvlist` with dotted keys (`a.b`), indexes arrays of
*structured* elements (`a[0].line`), and deliberately keeps arrays of *scalars* as one rendered
`[x,y]` pair — splitting those destroys the reading of things like permission-suggestion lists.

### Decoded output, real E2E (`otlp-min`, all variants exercised)

```
LOG  peer=127.0.0.1:64510 t=1788284877527324100 sev=SEVERITY_NUMBER_UNSPECIFIED
     scope=com.anthropic.claude_code.events@2.1.248 event_name_field=<EMPTY>
     body="claude_code.tool_result" trace=4bf92f3577b34da6a3ce929d0e0e4736 span=00f067aa0ba902b7
   | resource: service.name=claude-code service.version=2.1.248 os.type=windows
               os.version=10.0.26200 host.arch=amd64
   | attrs: event.name=tool_result event.timestamp=2026-09-01T17:40:38.740Z event.sequence=17
            session.id=4a892b7b-… prompt.id=1baff33e-… tool_name=Edit tool_use_id=toolu_01abcDEF
            success=true duration_ms=42 decision_type=accept decision_source=config
            tool_input_size_bytes=148 tool_result_size_bytes=96
            tool_input={"file_path":"C:\\coding\\agentolis\\src\\auth\\session.rs","old_string":"fn a()",…}
            tool_parameters={"bash_command":"cargo test","full_command":"cargo test --all",…}
            nested.kvlist.inner=x nested.kvlist.n=3 nested.kvlist.flag=true
            scalar.array=[Edit(src/**),Write(src/**)]
            structured.array[0].line=17 structured.array[0].op=replace
            cost_usd=0.0031 bytes.attr=0xdeadbeef empty.anyvalue= absent.value= nan.attr=NaN
     -> tool_input paths: ["C:\coding\agentolis\src\auth\session.rs"]
```

---

## 4. Metrics, and the temporality answer

### 4.1 Aggregation temporality: **DELTA**

This was the outstanding item. **Claude Code exports every Sum and Histogram as `AGGREGATION_TEMPORALITY_DELTA`
(enum value 1).** Measured, not inferred — all 13 real data points across captures B and C:

```
MET t=1788285012549000000 start=1788285009551000000 name=claude_code.session.count
    kind=counter temporality=DELTA unit=- value=1 scope=com.anthropic.claude_code@2.1.248
    attrs: session.id=54467fd5-… start_type=fresh

MET t=1788284438772000000 name=claude_code.token.usage  kind=counter temporality=DELTA unit=tokens
    value=17628  attrs: … model=claude-haiku-4-5-20251001 query_source=main type=cacheRead
MET t=1788284438772000000 name=claude_code.cost.usage   kind=counter temporality=DELTA unit=USD
    value=0.0182988  attrs: … model=claude-haiku-4-5-20251001 query_source=main
MET t=1788284438772000000 name=claude_code.active_time.total kind=counter temporality=DELTA unit=s
    value=1.105  attrs: … type=cli
```

`start_time_unix_nano` and `time_unix_nano` bracket the export window (3.0 s apart, matching
`OTEL_METRIC_EXPORT_INTERVAL=3000`).

This is governed by `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE`, **default `delta`**. Documented in
`otel.md` line 118 and now confirmed on the wire.

**What this means for Polis, and it is not a detail:**

- Each export carries *only what happened since the last export*. A `token.usage` of 17628 is 17628 tokens in
  the last 3 seconds, **not** a running total.
- Polis must **accumulate** these itself, per `(session.id, metric name, attribute set)`. Treating them as
  cumulative gauges makes every counter appear to sawtooth toward zero.
- Conversely, cumulative-style logic (`current - previous`) would double-count.
- Polis controls the env it injects, so it should **pin the preference explicitly** rather than rely on the
  default:
  `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE=delta`.
  Delta is the right choice — it is drop-tolerant in the sense Polis needs (a lost export loses one window, it
  does not corrupt the series baseline) and it needs no reset detection.
- The counter must still be written defensively: the receiver must handle `CUMULATIVE` (2) and `UNSPECIFIED`
  (0) without producing nonsense, because a user's managed settings can flip the preference.

### 4.2 Instrument types: **all eight metrics are counters. There are no histograms.**

Both the documented list and the live capture agree: `session.count`, `lines_of_code.count`,
`pull_request.count`, `commit.count`, `cost.usage`, `token.usage`, `code_edit_tool.decision`,
`active_time.total` — every one is a monotonic `Sum`.

Two consequences:

- **`claude_code.active_time.total` is a counter, not a gauge**, despite the `.total` name. It arrived as
  `Sum { is_monotonic: true, aggregation_temporality: DELTA }` with unit `s`. Read it as "seconds of active
  time accrued in this window".
- **Gauge / Histogram / ExponentialHistogram / Summary handling is purely defensive.** Implement all of them
  (the PRD's "unknown types logged at debug and dropped, never fatal" applies), but nothing on this channel
  produces them today. The E2E covers them with synthetic points; treat any real histogram as a schema change
  worth alerting on.

`metric::Data` is an `Option`, so `None` is reachable when a future OTLP release adds a variant this build does
not know. That arm must be handled, not `unwrap`ped.

### 4.3 `tool_input` is a JSON string — the path-extraction trap

This is the highest-consequence finding for territory inference, and it is easy to get wrong.

`tool_input`, `tool_parameters`, and `workspace.host_paths` are **not** OTLP `kvlist` values. They are
`string_value` holding **serialized JSON**. OTLP attribute flattening therefore cannot see inside them — a
receiver that only flattens attributes gets `tool_input` as one opaque blob and **never sees a single file
path**.

Worse on Windows: the paths inside are JSON-escaped, so `C:\coding\…` is on the wire as
`"C:\\coding\\…"`. Capture A proved this: an early probe that string-sliced on `"file_path":"` without
JSON-decoding produced `C:\\Users\\konka\\…` with doubled separators, which would have poisoned every path key
in the world model.

**Polis must `serde_json::from_str` the `tool_input` attribute and walk the resulting value.** The reference
`tool_input_paths` in §9 does this, recursing for `file_path`, `notebook_path`, `path`, `target_file`. Verified
end-to-end: the E2E line above recovers `C:\coding\agentolis\src\auth\session.rs` with correct single
separators.

Also note `tool_input` requires `OTEL_LOG_TOOL_DETAILS=1` (already in the PRD block) and that individual values
are truncated at 512 chars with the payload bounded to ~4 KB. Long `Write` bodies will be cut; use the FS
watcher (§4.3 of the PRD) for content, OTel for attribution.

---

## 5. Backpressure, port conflict, shutdown — measured

### 5.1 Backpressure never reaches the agent (PRD §4.5)

Stress test: 2000 sequential unary `Export` RPCs (4000 log records) into a **capacity-8** channel with **no
consumer at all** (`--capacity 8 --starve`).

```
CLIENT: 2000 log exports (4000 records) into a capacity-8 starved queue took 1793 ms; exit=0
LOG  batch rows=2 handler_us=118 dropped_total=0      <- first
LOG  batch rows=2 handler_us=59  dropped_total=3992   <- last
SHUTDOWN clean. log_rows=0 metric_rows=0 queued=8 dropped=3996
```

Handler latency across all 2000 calls, with the queue permanently full:

```
n=2000  min=36us  p50=50us  p90=67us  p99=99us  max=158us
```

- The handler **never blocks, never awaits a send, and always returns `Ok`.** p99 is 99 µs whether the queue is
  empty or full — the full path costs nothing extra.
- Accounting is exact: 3996 dropped + 8 still queued = 4004 = 4000 log records + 4 metric points.
- The client saw zero errors and zero stalls.

Two design rules this encodes:

1. **Never return an error `Status` for a full queue.** The OTel exporter retries on error, so a
   `RESOURCE_EXHAUSTED` is backpressure with extra steps — it would push work back at the agent, which §4.5
   forbids absolutely. Full success, always.
2. **PRD §4.5 says drop *oldest*.** `crossbeam::bounded` + `try_send` drops the *newest* on `Full`. To get
   drop-oldest, the sink holds its own `Receiver` clone and calls `try_recv()` to evict before retrying. This
   is sound because crossbeam channels are MPMC; losing the eviction race to the world thread just means it
   consumed the element that was going to be dropped anyway. The retry loop is bounded at 2 iterations so the
   handler can never spin.

### 5.2 Port 4317 already in use — degrade, never panic

The trick is **binding synchronously on the calling thread before spawning anything.** A `std::net::TcpListener::bind`
returns a plain `io::Error` you can match; the same failure inside a `#[tokio::main]` background thread is a panic
nobody is joining.

Proven three ways, including once by accident against a genuinely orphaned process from an earlier run:

```
WARN  polis: OTLP endpoint 127.0.0.1:4317 is already in use (Normalerweise darf jede Socketadresse
      (Protokoll, Netzwerkadresse oder Anschluss) nur jeweils einmal verwendet werden. (os error 10048)).
      Another Polis instance or an OTel collector is probably running. Continuing WITHOUT the
      OpenTelemetry channel; hooks, FS watch and JSONL tailing are unaffected.
DEGRADED second bind refused, no panic
```

Two Windows specifics:

- The error is **WSAEADDRINUSE, `os error 10048`**, and `ErrorKind::AddrInUse` matches it correctly.
- **The OS error string is localized.** On this box it is German. Never parse the message text — match on
  `ErrorKind`, and be aware that any log scraping of these warnings will see non-English text.

Polis must keep running: Channels B/C/D are independent, and a Polis that dies because a stale collector holds
4317 is worse than a Polis with no OTel.

### 5.3 Graceful shutdown

`serve_with_incoming_shutdown(incoming, async { let _ = shutdown_rx.await; })` with a `tokio::sync::oneshot`.
`ReceiverHandle::shutdown()` signals then joins the runtime thread; it is idempotent and `Drop` calls it, so a
panic in the UI thread still unwinds into a clean socket close. Verified: `SHUTDOWN clean.` prints after the
join, and the port is immediately rebindable.

---

## 6. The integration pattern Polis needs

### 6.1 The UI owns the main thread

winit/wgpu require the event loop on the main thread on Windows and macOS, so the OTLP server **cannot** use
`#[tokio::main]`. The shape is:

1. **Bind eagerly, on the caller's thread** → `AddrInUse` becomes a warning, not a panic (§5.2).
2. `set_nonblocking(true)`, then hand the `std::net::TcpListener` to `tokio::net::TcpListener::from_std` *inside*
   the runtime.
3. Build a private `new_multi_thread()` runtime with **2 worker threads** on a named background thread. Two is
   plenty: the handler is ~50 µs of pure decode and the traffic is one batch per second per agent.
4. `rt.block_on(server_future)` — the runtime lives and dies with that thread.
5. Return a `ReceiverHandle` holding the shutdown sender and the `JoinHandle`.

The main thread never sees `async`, never links `#[tokio::main]`, and drains the world-state channel with a
plain blocking `recv_timeout`.

Full compiled source in §9.

### 6.2 Sizing and limits

- `max_decoding_message_size(16 MiB)` on both services. Claude Code batches, and `tool_input` payloads
  (~4 KB each) plus a slow consumer can make a batch large. tonic's default is 4 MiB.
- `accept_compressed(CompressionEncoding::Gzip)` — costs one feature flag and avoids a mystery
  `Unimplemented` if `OTEL_EXPORTER_OTLP_COMPRESSION=gzip` is ever set.
- Bind **`127.0.0.1:4317`, not `0.0.0.0`.** The stream carries `user.email`, prompts, and file paths. There is
  no auth on this endpoint.

---

## 7. Documented vs. observed — disagreements and gaps

Cross-checked against `otel.md` (`code.claude.com/docs/en/monitoring-usage`).

**Nothing in the documentation was contradicted by the wire.** Every observed event name, metric name, resource
attribute and standard attribute matches. The gaps are omissions and traps:

| # | Item | Documented | Observed | Impact |
|---|---|---|---|---|
| 1 | Logs instrumentation scope | not documented (only the meter name is) | `com.anthropic.claude_code.events` @ `2.1.248` | Don't dispatch on scope; it is unspecified and free to move |
| 2 | `severity_number` / `severity_text` | not mentioned | always `0` / `""` | A severity filter drops 100% of traffic |
| 3 | OTLP `event_name` field (1.7, field 12) | not mentioned | always empty | Use `body`; the obvious field is the wrong one |
| 4 | `session.id` placement | listed under "Standard attributes", which are per-record | confirmed per-record; **absent from the resource block** | Resource-keyed grouping cannot separate agents |
| 5 | `query_source` vocabulary | *metrics*: closed set `main`/`subagent`/`auxiliary`; *events*: free-form subsystem string | metrics: `main`, `auxiliary`; events: `sdk`, `generate_session_title` | Same attribute name, two vocabularies across signals. Do not build one enum for both |
| 6 | Attribute wire types | not documented anywhere | `duration_ms` and `prompt_length` are **strings**; `safe_mode` is a string while `has_hooks` is a real `bool` **in the same event** | The biggest silent-data-loss risk on this channel. Coerce by key, accept any scalar variant (§2.4) |
| 7 | `active_time.total` instrument type | not stated | monotonic `Sum`, DELTA | The `.total` name reads like a gauge; it is not |
| 8 | Aggregation temporality | only via `OTEL_..._TEMPORALITY_PREFERENCE` default `delta` | DELTA on every point | Counters must be accumulated by Polis (§4.1) |
| 9 | `effort` attribute | documented on cost/token counters | observed `effort=xhigh` | fine |
| 10 | `event.sequence` | documented per event | integer, per-session, from 0 | Free gap detection; PRD does not mention it |

---

## 8. PRD corrections

**C1 — §4.1: metrics are DELTA; Polis must accumulate.**
The PRD says the OTel channel "carries the high-frequency traffic: tool calls, API requests, token counts" but
says nothing about temporality. Token and cost counters arrive as **per-window deltas**. Add to §4.1: pin
`OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE=delta` in the injected env, and state that the world model
accumulates counters per `(session.id, metric, attribute set)`.

**C2 — §4.1: `session.id` is not a resource attribute.**
Add an explicit note. The natural OTLP idiom (one resource per producer, group by resource) does not identify a
Claude Code session. Every record and data point must be routed by its own `session.id` attribute.

**C3 — §4.1 / §4.3 / §11.3: file paths require JSON parsing, not attribute flattening.**
The PRD treats OTel tool events as the attribution source for FS-watch correlation. But `tool_input` is a JSON
*string*; a receiver that only flattens OTLP attributes recovers **zero** file paths. Add: the receiver
JSON-decodes `tool_input` / `tool_parameters`, and on Windows must decode escapes rather than string-slice.
This compounds the hooks finding that `FileChanged` cannot attribute — JSON-decoding `tool_input` is now the
*only* reliable path→agent link on the OTel channel.

**C4 — §4.1: the `prompt.id` VERIFY item is resolved, with a caveat.**
`prompt.id` correlates a `user_prompt` with all following events **in that turn**. It is a turn key, not a
session key, and it is **absent on all startup traffic** (`plugin_loaded`, initial `mcp_server_connection`, the
session-title `api_request`). Do not use it as a join key for session-level state.

**C5 — §4.1: add `event.sequence` to the design.**
Undocumented in the PRD, present on every event, monotonic per session from 0. This is the cheapest possible
detector for the drops §4.5 explicitly permits. Track the high-water mark per session and surface holes in the
same status-bar counter as the dropped-events count.

**C6 — §4.1: "unknown event types … dropped, never fatal" needs to extend to the value layer.**
The PRD's defensiveness is scoped to event *types*. The real fragility is one level down, and it is measured
(§2.4): `duration_ms` and `prompt_length` are `string_value`, not numbers; `safe_mode` is a string while
`has_hooks` is a real `bool_value` **in the same event**. Add to §4.1: attribute values must be coerced by key
accepting any scalar variant, and the decoder must also survive empty `AnyValue`s, absent `value` fields, and
the `string_value_strindex` oneof arm. A decoder that matches one variant per key loses fields silently — no
error, no log line, just a missing number.

**C7 — §4.5: `crossbeam` `try_send` drops newest, but the PRD demands oldest.**
"On full, drop oldest" is not what a bounded crossbeam channel does by default. The sink must hold a `Receiver`
clone and evict via `try_recv()` before retrying. Worth stating in the PRD so nobody "simplifies" it back to a
plain `try_send`.

**C8 — §4.1: bind failure is a supported state, not an error path.**
The PRD does not say what happens when 4317 is taken. Specify: log a warning, continue without Channel A, and
surface the degraded state in the status bar. Never panic, never retry-loop, never pick a different port
(agents are configured to talk to 4317).

**C9 — §4.1: drop the `trace` feature; register two services, not three.**
`opentelemetry-proto`'s `trace` feature and `TraceServiceServer` are unnecessary unless beta tracing is
adopted. Also note that `tonic` 0.14 split codegen into `tonic-prost`, which is *not* a direct dependency.

**C10 — §4.1: PII on the wire.**
`user.email`, `user.account_id`, `user.account_uuid` and `organization.id` arrive on every record. The PRD is
silent on this. Specify that Polis drops `user.email` at ingest unless explicitly enabled, and that the
endpoint binds loopback-only.

---

## 9. Reference implementation

Compile-verified with the §1.1 block, zero warnings, `#![forbid(unsafe_code)]`. Sources:

- `<scratch>/otlp-min/src/lib.rs` — decoding
- `<scratch>/otlp-min/src/bin/server.rs` — runtime + sink
- `<scratch>/otlp-min/src/bin/client.rs` — replay client used for the E2E

### 9.1 Attribute extraction

```rust
pub use opentelemetry_proto::tonic as otlp;
use otlp::common::v1::{any_value::Value, AnyValue, KeyValue};

/// Every variant of the AnyValue oneof, including the profiling-only
/// StringValueStrindex (tag 8) and the absent case. Total; never panics.
pub fn any_value_to_string(v: &AnyValue) -> String {
    match &v.value {
        None => String::new(),
        Some(Value::StringValue(s)) => s.clone(),
        Some(Value::BoolValue(b)) => b.to_string(),
        Some(Value::IntValue(i)) => i.to_string(),
        Some(Value::DoubleValue(d)) => format_f64(*d),
        Some(Value::BytesValue(b)) => format!("0x{}", hex_lower(b)),
        Some(Value::ArrayValue(a)) => {
            let inner: Vec<String> = a.values.iter().map(any_value_to_string).collect();
            format!("[{}]", inner.join(","))
        }
        Some(Value::KvlistValue(kv)) => {
            let inner: Vec<String> = kv.values.iter().map(|e| {
                let r = e.value.as_ref().map(any_value_to_string).unwrap_or_default();
                format!("{}={}", e.key, r)
            }).collect();
            format!("{{{}}}", inner.join(","))
        }
        // Profiling-only. Never in Logs/Metrics; must stay non-fatal.
        Some(Value::StringValueStrindex(ix)) => format!("<strindex:{ix}>"),
    }
}

/// Nested kvlists flatten to dotted keys; arrays of structured elements get
/// bracket indices; arrays of scalars stay as ONE `[a,b]` pair.
pub fn flatten_attributes(attrs: &[KeyValue], prefix: &str, out: &mut Vec<(String, String)>) {
    for kv in attrs {
        let key = if prefix.is_empty() { kv.key.clone() }
                  else { format!("{prefix}.{}", kv.key) };
        match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
            Some(Value::KvlistValue(list)) => flatten_attributes(&list.values, &key, out),
            Some(Value::ArrayValue(arr)) if arr.values.iter().any(is_structured) => {
                for (i, elem) in arr.values.iter().enumerate() {
                    let ikey = format!("{key}[{i}]");
                    match &elem.value {
                        Some(Value::KvlistValue(l)) => flatten_attributes(&l.values, &ikey, out),
                        _ => out.push((ikey, any_value_to_string(elem))),
                    }
                }
            }
            Some(_) => out.push((key, any_value_to_string(kv.value.as_ref().unwrap()))),
            None => out.push((key, String::new())),
        }
    }
}

fn is_structured(v: &AnyValue) -> bool {
    matches!(v.value, Some(Value::KvlistValue(_)) | Some(Value::ArrayValue(_)))
}

/// Fetch an attribute Claude Code encodes as a JSON *string*.
/// `tool_input`, `tool_parameters`, `workspace.host_paths` are NOT kvlists.
/// On Windows the embedded paths are JSON-escaped (`C:\\Users\\x`) and MUST be
/// JSON-decoded, not string-sliced.
pub fn json_string_attr<'a>(attrs: &'a [KeyValue], key: &str) -> Option<&'a str> {
    attrs.iter().find(|kv| kv.key == key).and_then(|kv| {
        match kv.value.as_ref()?.value.as_ref()? {
            Value::StringValue(s) => Some(s.as_str()),
            _ => None,
        }
    })
}

/// Recover file paths from the tool_input JSON. This is the only reliable
/// path -> agent link on the OTel channel.
pub fn tool_input_paths(tool_input_json: &str) -> Vec<String> {
    const PATH_KEYS: [&str; 4] = ["file_path", "notebook_path", "path", "target_file"];
    let Ok(v) = serde_json::from_str::<serde_json::Value>(tool_input_json) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_paths(&v, &PATH_KEYS, &mut out);
    out
}

fn collect_paths(v: &serde_json::Value, keys: &[&str], out: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map {
                if keys.contains(&k.as_str()) {
                    if let Some(s) = val.as_str() { out.push(s.to_string()); continue; }
                }
                collect_paths(val, keys, out);
            }
        }
        serde_json::Value::Array(items) => for it in items { collect_paths(it, keys, out) },
        _ => {}
    }
}

/// Coerce a rendered attribute to a number regardless of how it was encoded.
/// Required: duration_ms and prompt_length arrive as strings (§2.4).
pub fn as_f64(rendered: &str) -> Option<f64> { rendered.parse::<f64>().ok() }

/// Same for booleans: bool_value and the strings "true"/"false" both occur,
/// in the same event.
pub fn as_bool(rendered: &str) -> Option<bool> {
    match rendered { "true" => Some(true), "false" => Some(false), _ => None }
}

/// Diagnostic: name the oneof arm of every attribute. Run this against a live
/// session after any Claude Code upgrade to catch wire-type drift.
pub fn variant_names(attrs: &[KeyValue]) -> Vec<(String, &'static str)> {
    attrs.iter().map(|kv| {
        let v = match kv.value.as_ref().and_then(|a| a.value.as_ref()) {
            None => "ABSENT",
            Some(Value::StringValue(_)) => "string",
            Some(Value::BoolValue(_)) => "bool",
            Some(Value::IntValue(_)) => "int",
            Some(Value::DoubleValue(_)) => "double",
            Some(Value::BytesValue(_)) => "bytes",
            Some(Value::ArrayValue(_)) => "array",
            Some(Value::KvlistValue(_)) => "kvlist",
            Some(Value::StringValueStrindex(_)) => "strindex",
        };
        (kv.key.clone(), v)
    }).collect()
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn format_f64(d: f64) -> String {
    if d.is_nan() { "NaN".into() }
    else if d.is_infinite() {
        if d.is_sign_positive() { "Infinity" } else { "-Infinity" }.into()
    } else if d.fract() == 0.0 && d.abs() < 1e15 { format!("{d:.0}") }
    else { format!("{d}") }
}
```

### 9.2 Walking Logs and Metrics (incl. temporality)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Temporality { Unspecified, Delta, Cumulative }

impl Temporality {
    pub fn from_i32(v: i32) -> Self {
        match v { 1 => Self::Delta, 2 => Self::Cumulative, _ => Self::Unspecified }
    }
}

/// ResourceLogs -> ScopeLogs -> LogRecord
pub fn walk_logs(req: &otlp::collector::logs::v1::ExportLogsServiceRequest) -> Vec<LogRow> {
    let mut rows = Vec::new();
    for rl in &req.resource_logs {
        let mut resource = Vec::new();
        if let Some(r) = &rl.resource { flatten_attributes(&r.attributes, "", &mut resource); }
        for sl in &rl.scope_logs {
            let (scope, scope_version) = sl.scope.as_ref()
                .map(|s| (s.name.clone(), s.version.clone())).unwrap_or_default();
            for lr in &sl.log_records {
                let mut attrs = Vec::new();
                flatten_attributes(&lr.attributes, "", &mut attrs);
                // tool_input is a JSON string; paths live inside it.
                let mut paths = Vec::new();
                for key in ["tool_input", "tool_parameters"] {
                    if let Some(js) = json_string_attr(&lr.attributes, key) {
                        paths.extend(tool_input_paths(js));
                    }
                }
                rows.push(LogRow {
                    resource: resource.clone(),
                    scope: scope.clone(), scope_version: scope_version.clone(),
                    time_unix_nano: lr.time_unix_nano,
                    observed_time_unix_nano: lr.observed_time_unix_nano,
                    // Always UNSPECIFIED from Claude Code — never filter on it.
                    severity: if lr.severity_text.is_empty() { severity_name(lr.severity_number) }
                              else { lr.severity_text.clone() },
                    event_name: lr.event_name.clone(),   // empty from Claude Code
                    body: lr.body.as_ref().map(any_value_to_string).unwrap_or_default(),
                    attrs, paths,
                    trace_id: hex_lower(&lr.trace_id),
                    span_id: hex_lower(&lr.span_id),
                });
            }
        }
    }
    rows
}

/// ResourceMetrics -> ScopeMetrics -> Metric, one row per data point.
/// Every metric::Data variant, plus the None arm a future OTLP release brings.
pub fn walk_metrics(
    req: &otlp::collector::metrics::v1::ExportMetricsServiceRequest,
) -> Vec<MetricRow> {
    use otlp::metrics::v1::{metric::Data, number_data_point};
    let mut rows = Vec::new();
    for rm in &req.resource_metrics {
        let mut resource = Vec::new();
        if let Some(r) = &rm.resource { flatten_attributes(&r.attributes, "", &mut resource); }
        for sm in &rm.scope_metrics {
            let (scope, scope_version) = sm.scope.as_ref()
                .map(|s| (s.name.clone(), s.version.clone())).unwrap_or_default();
            for m in &sm.metrics {
                let mut push = |kind: &'static str, value: Option<f64>, count: Option<u64>,
                                pt_attrs: &[KeyValue], start: u64, t: u64, temporality: i32| {
                    let mut attrs = Vec::new();
                    flatten_attributes(pt_attrs, "", &mut attrs);
                    rows.push(MetricRow {
                        resource: resource.clone(),
                        scope: scope.clone(), scope_version: scope_version.clone(),
                        name: m.name.clone(), unit: m.unit.clone(), kind, value, count, attrs,
                        start_time_unix_nano: start, time_unix_nano: t,
                        temporality: Temporality::from_i32(temporality),
                    });
                };
                let num = |v: &Option<number_data_point::Value>| match v {
                    Some(number_data_point::Value::AsDouble(d)) => Some(*d),
                    Some(number_data_point::Value::AsInt(i))    => Some(*i as f64),
                    None => None,
                };
                match &m.data {
                    Some(Data::Gauge(g)) => for p in &g.data_points {
                        push("gauge", num(&p.value), None, &p.attributes,
                             p.start_time_unix_nano, p.time_unix_nano, 0);
                    },
                    Some(Data::Sum(s)) => {
                        let kind = if s.is_monotonic { "counter" } else { "updowncounter" };
                        for p in &s.data_points {
                            push(kind, num(&p.value), None, &p.attributes,
                                 p.start_time_unix_nano, p.time_unix_nano,
                                 s.aggregation_temporality);   // DELTA from Claude Code
                        }
                    }
                    Some(Data::Histogram(h)) => for p in &h.data_points {
                        push("histogram", p.sum, Some(p.count), &p.attributes,
                             p.start_time_unix_nano, p.time_unix_nano, h.aggregation_temporality);
                    },
                    Some(Data::ExponentialHistogram(h)) => for p in &h.data_points {
                        push("exp_histogram", p.sum, Some(p.count), &p.attributes,
                             p.start_time_unix_nano, p.time_unix_nano, h.aggregation_temporality);
                    },
                    Some(Data::Summary(s)) => for p in &s.data_points {
                        push("summary", Some(p.sum), Some(p.count), &p.attributes,
                             p.start_time_unix_nano, p.time_unix_nano, 0);
                    },
                    // Unknown / future variant: record it, never fail.
                    None => push("unknown", None, None, &[], 0, 0, 0),
                }
            }
        }
    }
    rows
}
```

### 9.3 Bounded drop-oldest sink (PRD §4.5)

```rust
use crossbeam_channel::{Receiver, Sender, TrySendError};
use std::sync::{atomic::{AtomicU64, Ordering}, Arc};

/// Holds its own Receiver clone purely to EVICT the oldest element when full.
/// crossbeam channels are MPMC, so racing the world thread is sound: losing
/// the race means it consumed the element we were going to drop anyway.
#[derive(Clone)]
pub struct Sink {
    tx: Sender<Ingested>,
    evict: Receiver<Ingested>,
    dropped: Arc<AtomicU64>,
}

impl Sink {
    pub fn new(capacity: usize) -> (Self, Receiver<Ingested>, Arc<AtomicU64>) {
        let (tx, rx) = crossbeam_channel::bounded::<Ingested>(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        (Sink { tx, evict: rx.clone(), dropped: dropped.clone() }, rx, dropped)
    }

    /// Never blocks. Never awaits. Bounded work regardless of queue state.
    fn push(&self, ev: Ingested) {
        let mut ev = ev;
        // Two attempts: one eviction makes room for one element. A second
        // failure means other producers refilled it -- drop and move on rather
        // than spin. Backpressure must never reach the agent.
        for _ in 0..2 {
            match self.tx.try_send(ev) {
                Ok(()) => return,
                Err(TrySendError::Full(returned)) => {
                    if self.evict.try_recv().is_ok() {          // PRD §4.5: drop OLDEST
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    ev = returned;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
        }
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }
}
```

The handler that uses it:

```rust
#[tonic::async_trait]
impl LogsService for LogsSvc {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        for row in walk_logs(&request.into_inner()) {
            self.sink.push(Ingested::Log(Box::new(row)));   // never awaits
        }
        // ALWAYS Ok. An error Status makes the exporter retry, which is
        // backpressure with extra steps. Measured p99 = 99us with a full queue.
        Ok(Response::new(ExportLogsServiceResponse { partial_success: None }))
    }
}
```

### 9.4 Background runtime, degrade-on-conflict, graceful shutdown

```rust
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::thread::JoinHandle;
use tonic::codec::CompressionEncoding;

pub struct ReceiverHandle {
    pub local_addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl ReceiverHandle {
    /// Idempotent: signal, then join. Drop calls it too.
    pub fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown.take() { let _ = tx.send(()); }
        if let Some(t)  = self.thread.take()   { let _ = t.join(); }
    }
}
impl Drop for ReceiverHandle {
    fn drop(&mut self) { self.shutdown(); }
}

/// Returns None (warning already logged) when the endpoint can't be bound.
/// Polis MUST keep running without OTel: hooks, FS watch and JSONL tailing
/// are independent channels.
pub fn spawn_receiver(addr: SocketAddr, sink: Sink) -> Option<ReceiverHandle> {
    // Bind SYNCHRONOUSLY on the calling thread. This is the whole trick: the
    // failure we care about (WSAEADDRINUSE 10048 / EADDRINUSE) surfaces here as
    // a plain io::Error we can match on, instead of as a panic buried in a
    // #[tokio::main] unwrap on a thread nobody is joining.
    let std_listener = match StdTcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!(
                "WARN  polis: OTLP endpoint {addr} is already in use ({e}). Another Polis \
                 instance or an OTel collector is probably running. Continuing WITHOUT the \
                 OpenTelemetry channel; hooks, FS watch and JSONL tailing are unaffected."
            );
            return None;   // NOTE: the OS message is localized. Match ErrorKind, never text.
        }
        Err(e) => {
            eprintln!("WARN  polis: could not bind OTLP endpoint {addr}: {e}. Continuing without OTel.");
            return None;
        }
    };
    if let Err(e) = std_listener.set_nonblocking(true) {
        eprintln!("WARN  polis: set_nonblocking failed on {addr}: {e}. Continuing without OTel.");
        return None;
    }
    let local_addr = std_listener.local_addr().ok()?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let thread = std::thread::Builder::new()
        .name("polis-otlp".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)              // handler is ~50us of pure decode
                .thread_name("polis-otlp-rt")
                .enable_io().enable_time()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => { eprintln!("WARN  polis: OTLP runtime build failed: {e}"); return; }
            };
            rt.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(std_listener) {
                    Ok(l) => l,
                    Err(e) => { eprintln!("WARN  polis: from_std failed: {e}"); return; }
                };
                let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

                let logs = LogsServiceServer::new(LogsSvc { sink: sink.clone() })
                    .max_decoding_message_size(16 * 1024 * 1024)
                    .accept_compressed(CompressionEncoding::Gzip);
                let metrics = MetricsServiceServer::new(MetricsSvc { sink })
                    .max_decoding_message_size(16 * 1024 * 1024)
                    .accept_compressed(CompressionEncoding::Gzip);

                let res = tonic::transport::Server::builder()
                    .add_service(logs)
                    .add_service(metrics)
                    .serve_with_incoming_shutdown(incoming, async { let _ = shutdown_rx.await; })
                    .await;
                if let Err(e) = res {
                    eprintln!("WARN  polis: OTLP server exited with error: {e}");
                }
            });
            // rt drops here, after graceful shutdown has drained connections.
        });

    match thread {
        Ok(thread) => Some(ReceiverHandle {
            local_addr, shutdown: Some(shutdown_tx), thread: Some(thread),
        }),
        Err(e) => {
            eprintln!("WARN  polis: could not spawn OTLP thread: {e}. Continuing without OTel.");
            None
        }
    }
}
```

Call site — the main thread stays synchronous:

```rust
fn main() {
    let (sink, rx, dropped) = Sink::new(65_536);            // PRD §4.5 capacity
    let addr: SocketAddr = ([127, 0, 0, 1], 4317).into();   // loopback ONLY: PII on the wire

    let mut otlp = spawn_receiver(addr, sink);              // None == degraded, not fatal
    if otlp.is_none() {
        // surface "OTel: off" in the status bar and carry on
    }

    run_ui_event_loop(rx, dropped);                         // winit owns the main thread

    if let Some(h) = otlp.as_mut() { h.shutdown(); }
}
```

---

## 10. Reproduction

```powershell
# terminal 1 — receiver
cargo run --bin otlp-min-server -- --port 4317 --seconds 60

# terminal 2 — synthetic replay (all AnyValue variants, both temporalities)
cargo run --bin otlp-min-client -- http://127.0.0.1:4317 1

# backpressure proof: capacity 8, no consumer, 2000 RPCs
cargo run --bin otlp-min-server -- --port 4317 --seconds 25 --capacity 8 --starve
cargo run --bin otlp-min-client -- http://127.0.0.1:4317 2000

# port-conflict proof
cargo run --bin otlp-min-server -- --port 4317 --seconds 12 --twice

# live Claude Code (one trivial call)
$env:CLAUDE_CODE_ENABLE_TELEMETRY="1"; $env:OTEL_LOGS_EXPORTER="otlp"
$env:OTEL_METRICS_EXPORTER="otlp"; $env:OTEL_EXPORTER_OTLP_PROTOCOL="grpc"
$env:OTEL_EXPORTER_OTLP_ENDPOINT="http://127.0.0.1:4317"
$env:OTEL_LOGS_EXPORT_INTERVAL="1000"; $env:OTEL_METRIC_EXPORT_INTERVAL="3000"
$env:OTEL_LOG_TOOL_DETAILS="1"
claude -p "Reply with exactly: ok" --model haiku

# zero-cost variant: session starts and emits metrics, API call refused
$env:ANTHROPIC_BASE_URL="http://127.0.0.1:9"
claude -p "x" --model haiku
```

---

## 11. Open items

1. ~~`tool_result` / `tool_decision` full attribute set not dumped verbatim.~~ **Closed by `otel-schema.md`
   §5.2–5.3**, which captured 9 real `tool_result` records including `Edit` with a 713-char `old_string`. Its
   independent findings match this doc's: `tool_input` is a JSON string, `file_path` is the first key inside
   it, and per-value truncation keeps the JSON valid. Two extra facts from that capture worth importing into
   the receiver: the real per-value truncation budget is **128 chars**, not the documented 512 (check for the
   `…[N chars]` marker before trusting any `tool_input` value except `file_path`), and `decision_type` /
   `decision_source` did **not** appear on any observed `tool_result` despite being documented — treat both as
   optional.
2. **No histogram has ever been observed.** Histogram/ExponentialHistogram/Summary handling is written and
   E2E-tested against synthetic points only.
3. **`double_value` was never seen on the wire.** The variant dump (§2.4) covered `plugin_loaded`,
   `permission_mode_changed`, `mcp_server_connection` and `user_prompt` and found only `string`, `int` and
   `bool`. `cost_usd` is the obvious `double` candidate but sits on `api_request`, which the zero-cost capture
   could not produce. Handle `double_value` regardless — it costs one match arm.
4. **`workspace.host_paths`** is documented as a string array attribute but was not observed (desktop-app
   sessions only). Whether it is an OTLP `ArrayValue` or a JSON string is unconfirmed; the decoder handles both.
5. **Multi-agent fan-in was not load-tested.** One session was measured. The p99 = 99 µs handler and the
   65536-deep channel leave enormous headroom, but a 20-agent fleet has not been run.
6. **`OTEL_METRIC_EXPORT_INTERVAL=10000`** (the PRD value) means counters lag by up to 10 s. If the UI shows
   live token burn, either shorten it or drive that display from the `api_request` *event* stream (1 s), which
   carries the same token counts per request.
