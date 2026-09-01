//! Channel A — the embedded OTLP/gRPC receiver (PRD §4.1).
//!
//! > Polis embeds an OTLP/gRPC receiver on 4317 (`tonic` + `opentelemetry-proto`).
//! > No external collector.
//!
//! This module owns the **transport**: binding, the tonic service
//! implementations, the private runtime, shutdown, and the walk down
//! `Resource* -> Scope* -> record` that turns one export request into
//! [`crate::normalize`]'s borrowed [`OtlpRecord`] / [`OtlpSpan`] views. Turning
//! one of those into a [`polis_events::Event`] belongs to [`crate::normalize`]
//! and lives there only — the decode rules are shared with the other three
//! channels and the PII scrubbing (ADR-0005) must exist in exactly one place.
//!
//! # Register three services, not two
//!
//! `LogsServiceServer`, `MetricsServiceServer` **and** `TraceServiceServer`.
//! `docs/verified/otlp-receiver.md` §1 concludes that traces are dead weight;
//! ADR-0006 and ADR-0045 overrule it, and the workspace enables the `trace`
//! feature accordingly. Omitting the third makes Claude Code's trace exports
//! fail `UNIMPLEMENTED` and costs Polis its thread model, because the logs
//! channel carries no subagent discriminator at all — `agent_id` occurs **zero**
//! times across every `tool_decision` and `tool_result`. If spans stop arriving,
//! attribute every tool call to the main agent and mark the session degraded;
//! never guess. That is what [`OtlpReceiver::has_seen_spans`] and the
//! [`SourceHealth::Degraded`] arm of [`OtlpReceiver::health`] exist for.
//!
//! Exact module paths (`docs/verified/otlp-receiver.md` §1.3):
//!
//! ```text
//! opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::{LogsService, LogsServiceServer}
//! opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_server::{MetricsService, MetricsServiceServer}
//! opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::{TraceService, TraceServiceServer}
//! ```
//!
//! Each trait has exactly one method, `async fn export`, implemented with
//! `#[tonic::async_trait]`.
//!
//! # Bind synchronously, then hand the listener to tokio
//!
//! `std::net::TcpListener::bind` on the calling thread, then `from_std` inside
//! the runtime. This is the whole trick: `AddrInUse` surfaces as a matchable
//! `io::Error` instead of a panic buried in a background task nobody joins.
//! Never retry-loop, and never pick a different port — agents are configured to
//! talk to 4317, so a different port yields an empty city that looks healthy.
//! The Windows error *string* is localised, so match on
//! [`std::io::ErrorKind::AddrInUse`], never on the message.
//!
//! # Backpressure is absolute (PRD §4.5, ADR-0008)
//!
//! Every `export` handler walks the batch, hands each event to the bounded bus
//! and returns `Ok` — it never awaits a send and never returns an error
//! `Status`. An error would make the OTel exporter *retry*, which is
//! backpressure with extra steps and is exactly what PRD §4.5 forbids. Measured
//! against a permanently full capacity-8 queue with no consumer: 2 000 RPCs, p99
//! handler latency 99 µs, zero errors.
//!
//! # Shutdown is graceful but **bounded**
//!
//! A live agent holds an idle HTTP/2 connection open for the life of its
//! session, and tonic's graceful shutdown waits for open connections to finish.
//! Waiting forever would hang Polis's own exit on a peer that has no reason to
//! hang up, so [`OtlpReceiver::shutdown`] signals, waits at most
//! [`SHUTDOWN_GRACE`], and then drops the runtime under whatever is left.
//!
//! # Three things that are easy to get quietly wrong
//!
//! * **`session.id` is not a resource attribute.** The resource block carries
//!   exactly five keys — `service.name`, `service.version`, `os.type`,
//!   `os.version`, `host.arch`. Grouping by resource, the normal OTLP idiom,
//!   cannot separate agents, so nothing here folds resource attributes into a
//!   record's own.
//! * **Counters are DELTA.** Every export is the increment since the last.
//!   Reading them as cumulative is a *silent* failure: totals collapse to "the
//!   last export interval" and still look plausible (ADR-0007). The temporality
//!   is read off the wire by [`crate::normalize::otlp_metric`], never assumed.
//! * **Dispatch on the record body, never on severity or `event_name`.**
//!   `severity_number` is `UNSPECIFIED` and the OTLP 1.7 `event_name` field is
//!   empty on *every* Claude Code record, so a receiver that filters on
//!   severity drops 100% of the traffic and one that keys on `event_name` sees
//!   nothing at all.

use std::collections::BTreeSet;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::{
    LogsService, LogsServiceServer,
};
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_server::{
    MetricsService, MetricsServiceServer,
};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::{
    TraceService, TraceServiceServer,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use opentelemetry_proto::tonic::resource::v1::Resource;

use polis_events::{Channel, Event, OtelEvent, PathMapper, Payload};

use crate::bus::EventSink;
use crate::normalize::{
    any_value_to_string, attr, drift, flatten_attributes, is_refused_event, otlp_metric,
    otlp_record, otlp_span, OtlpRecord, OtlpSpan, CLAUDE_CODE_SERVICE,
};
use crate::{IngestSource, SourceHealth};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The prefix every Claude Code event, metric and span name carries.
///
/// The name is in the `LogRecord` **body** fully qualified
/// (`claude_code.tool_result`) and duplicated unprefixed in the `event.name`
/// attribute. [`polis_events::OTEL_EVENT_NAMES`] holds the unprefixed spelling,
/// so the body is stripped before dispatch.
const CLAUDE_PREFIX: &str = "claude_code.";

/// Batches are modest in practice, but tonic's 4 MiB default is tight for a slow
/// consumer plus `tool_input` payloads; both recon documents used 16 MiB.
const MAX_DECODING_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// The handler is ~50 µs of pure decode and traffic is roughly one batch per
/// second per agent, so two workers is ample.
const RUNTIME_WORKER_THREADS: usize = 2;

/// How long the receiver waits for the first beta span before reporting degraded
/// subagent attribution.
///
/// `OTEL_TRACES_EXPORT_INTERVAL` is 1 s in the injected env (ADR-0006), so ten
/// seconds is a wide margin and cannot flap merely because a log batch beat the
/// first trace batch.
const SPAN_GRACE: Duration = Duration::from_secs(10);

/// How long shutdown lets open connections drain before the runtime is dropped
/// under them.
///
/// An agent's exporter keeps one idle HTTP/2 connection per session and will
/// happily hold it open across Polis's whole exit; an unbounded graceful
/// shutdown therefore hangs the process. Two seconds is far more than a `GOAWAY`
/// round trip on loopback.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Distinct unrecognised names carried in one drift report.
const MAX_DRIFT_NAMES: usize = 8;

/// Characters of an unrecognised name echoed into a drift report.
const MAX_DRIFT_NAME_CHARS: usize = 64;

// ---------------------------------------------------------------------------
// Walking the three request shapes
// ---------------------------------------------------------------------------

/// Walks `ResourceLogs -> ScopeLogs -> LogRecord`, appending bus events.
///
/// Dispatch is on the record **body** (see the module docs). A body name outside
/// [`polis_events::OTEL_EVENT_NAMES`] still produces an event —
/// [`polis_events::OtelEvent::Unknown`], because a silently dropped record is
/// what PRD §17's schema-drift warning exists to prevent — and the batch also
/// gets **one** [`polis_events::ControlEvent::SchemaDrift`] naming what was not
/// recognised. One per batch, not one per record: the port is unauthenticated,
/// and a noisy neighbour must not be able to turn the bus into an amplifier.
///
/// Returns the number of events appended to `out`.
pub fn walk_logs(
    request: &ExportLogsServiceRequest,
    mapper: &PathMapper,
    out: &mut Vec<Event>,
) -> usize {
    let before = out.len();
    let mut drifted = DriftLog::default();
    for resource_logs in &request.resource_logs {
        let resource = resource_attributes(resource_logs.resource.as_ref());
        let service_name = attr(&resource, "service.name");
        let service_version = attr(&resource, "service.version");
        if is_foreign(service_name) {
            continue;
        }
        for scope_logs in &resource_logs.scope_logs {
            for log_record in &scope_logs.log_records {
                let mut attributes = Vec::new();
                flatten_attributes(&log_record.attributes, &mut attributes);
                let body_name = record_name(log_record, &attributes);
                if is_refused_event(&body_name) {
                    // Only reachable when someone set OTEL_LOG_RAW_API_BODIES:
                    // these two carry entire conversation histories (ADR-0005).
                    tracing::warn!(
                        event = %body_name,
                        "refusing a raw API body record; OTEL_LOG_RAW_API_BODIES is set somewhere"
                    );
                    continue;
                }
                let trace_id = hex_lower(&log_record.trace_id);
                let span_id = hex_lower(&log_record.span_id);
                let record = OtlpRecord {
                    body_name: &body_name,
                    attributes: &attributes,
                    trace_id: non_empty(&trace_id),
                    span_id: non_empty(&span_id),
                    service_name,
                    service_version,
                };
                if let Some(event) = otlp_record(&record, mapper) {
                    if let Some(name) = unknown_name(&event) {
                        tracing::debug!(event = %name, "unmodelled claude_code event name");
                        drifted.note(name, service_version);
                    }
                    out.push(event);
                }
            }
        }
    }
    drifted.drain_into("claude_code event name(s)", out);
    out.len() - before
}

/// Walks `ResourceMetrics -> ScopeMetrics -> Metric`, appending one event per
/// data point. Returns the number of events appended.
///
/// [`crate::normalize::otlp_metric`] never sees the resource block, so the
/// foreign-traffic filter has to happen here.
pub fn walk_metrics(request: &ExportMetricsServiceRequest, out: &mut Vec<Event>) -> usize {
    let before = out.len();
    for resource_metrics in &request.resource_metrics {
        let resource = resource_attributes(resource_metrics.resource.as_ref());
        if is_foreign(attr(&resource, "service.name")) {
            continue;
        }
        for scope_metrics in &resource_metrics.scope_metrics {
            for metric in &scope_metrics.metrics {
                otlp_metric(metric, out);
            }
        }
    }
    out.len() - before
}

/// Walks `ResourceSpans -> ScopeSpans -> Span`, appending bus events.
///
/// Spans arrive **before their parents** — OTLP batches are not topologically
/// ordered — so nothing here resolves parentage. Each span carries its own
/// `span_id` and `parent_span_id` and `polis-world` links them lazily
/// (ADR-0014). Unrecognised span names are reported exactly as unrecognised
/// event names are, in one drift report per batch.
///
/// Returns the number of events appended.
pub fn walk_traces(
    request: &ExportTraceServiceRequest,
    mapper: &PathMapper,
    out: &mut Vec<Event>,
) -> usize {
    let before = out.len();
    let mut drifted = DriftLog::default();
    for resource_spans in &request.resource_spans {
        let resource = resource_attributes(resource_spans.resource.as_ref());
        let service_name = attr(&resource, "service.name");
        let service_version = attr(&resource, "service.version");
        if is_foreign(service_name) {
            continue;
        }
        for scope_spans in &resource_spans.scope_spans {
            for span in &scope_spans.spans {
                let mut attributes = Vec::new();
                flatten_attributes(&span.attributes, &mut attributes);
                let span_id = hex_lower(&span.span_id);
                let parent_span_id = hex_lower(&span.parent_span_id);
                let trace_id = hex_lower(&span.trace_id);
                let decoded = OtlpSpan {
                    name: &span.name,
                    span_id: &span_id,
                    parent_span_id: non_empty(&parent_span_id),
                    trace_id: &trace_id,
                    attributes: &attributes,
                    // Saturating, because a producer is free to send an end
                    // before its start and a wrapped duration is nonsense.
                    duration_nanos: span
                        .end_time_unix_nano
                        .saturating_sub(span.start_time_unix_nano),
                };
                if let Some(event) = otlp_span(&decoded, mapper) {
                    if let Some(name) = unknown_name(&event) {
                        tracing::debug!(span = %name, "unmodelled claude_code span name");
                        drifted.note(name, service_version);
                    }
                    out.push(event);
                }
            }
        }
    }
    drifted.drain_into("claude_code span name(s)", out);
    out.len() - before
}

/// The event name, from the record body with the `claude_code.` prefix stripped.
///
/// The body is present on every record and is the only reliable source. The
/// `event.name` attribute is the documented cross-check and carries the same
/// name already unprefixed; the OTLP `event_name` field is empty on every Claude
/// Code record and is tried last only so that a future release which starts
/// populating it is not a silent blackout.
fn record_name(log_record: &LogRecord, attributes: &[(String, String)]) -> String {
    let body = log_record
        .body
        .as_ref()
        .map_or_else(String::new, any_value_to_string);
    let stripped = body.strip_prefix(CLAUDE_PREFIX).unwrap_or(&body);
    if !stripped.is_empty() {
        return stripped.to_owned();
    }
    if let Some(name) = attr(attributes, "event.name").filter(|n| !n.is_empty()) {
        return name.to_owned();
    }
    log_record
        .event_name
        .strip_prefix(CLAUDE_PREFIX)
        .unwrap_or(&log_record.event_name)
        .to_owned()
}

fn resource_attributes(resource: Option<&Resource>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(resource) = resource {
        flatten_attributes(&resource.attributes, &mut out);
    }
    out
}

/// True for telemetry that is definitely not Claude Code's.
///
/// Port 4317 on loopback is unauthenticated: any local process can post
/// arbitrary OTLP. A *declared* `service.name` other than
/// [`CLAUDE_CODE_SERVICE`] is foreign and is dropped whole, resource by
/// resource. An **absent** name is passed through rather than dropped, so that a
/// release which stops sending the resource block degrades into
/// [`crate::normalize`]'s hands instead of into an empty city that looks healthy
/// (ADR-0011).
fn is_foreign(service_name: Option<&str>) -> bool {
    let foreign = service_name.is_some_and(|name| name != CLAUDE_CODE_SERVICE);
    if foreign {
        tracing::debug!(
            service = service_name.unwrap_or_default(),
            "dropping foreign OTLP traffic on the Polis endpoint"
        );
    }
    foreign
}

/// The wire name behind an [`OtelEvent::Unknown`], if that is what this is.
fn unknown_name(event: &Event) -> Option<&str> {
    match &event.payload {
        Payload::Otel(body) => match &**body {
            OtelEvent::Unknown { name } => Some(name.as_str()),
            _ => None,
        },
        _ => None,
    }
}

/// One drift report per export batch, bounded in both count and length.
#[derive(Debug, Default)]
struct DriftLog {
    names: BTreeSet<String>,
    version: Option<String>,
}

impl DriftLog {
    fn note(&mut self, name: &str, version: Option<&str>) {
        if self.version.is_none() {
            self.version = version.filter(|v| !v.is_empty()).map(ToOwned::to_owned);
        }
        if self.names.len() < MAX_DRIFT_NAMES {
            // `chars`, not a byte slice: a hostile name is free to be UTF-8, and
            // slicing one by a byte index panics.
            self.names
                .insert(name.chars().take(MAX_DRIFT_NAME_CHARS).collect());
        }
    }

    fn drain_into(self, what: &str, out: &mut Vec<Event>) {
        if self.names.is_empty() {
            return;
        }
        // A BTreeSet, so the report reads the same for the same batch whatever
        // order the records arrived in.
        let joined: Vec<String> = self.names.into_iter().collect();
        out.push(drift(
            Channel::Otel,
            self.version.as_deref(),
            format!("unknown {what}: {}", joined.join(", ")),
        ));
    }
}

fn non_empty(value: &str) -> Option<&str> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Lowercase hex for a `trace_id` / `span_id`, which are raw bytes on the wire.
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Where decoded events go.
///
/// The production implementor is [`EventSink`]. It exists so the tonic service
/// implementations can be driven directly in tests — over a real socket, with
/// the real decode path — without standing up the whole bus.
pub(crate) trait EventOut: Send + Sync + 'static {
    /// Hands one event on. Must never block and must never fail: it is called
    /// from inside a gRPC handler, and blocking there is backpressure reaching
    /// an agent (PRD §4.5).
    fn emit(&self, event: Event);
}

impl EventOut for EventSink {
    fn emit(&self, event: Event) {
        // The return value says whether the bus evicted its oldest entry. That
        // is already counted in `BusStats` and surfaced in the status bar; there
        // is nothing useful to do about it here, and certainly nothing to
        // propagate back to the agent.
        let _ = self.push(event);
    }
}

/// Counters the three handlers share with [`OtlpReceiver::health`].
#[derive(Debug, Default)]
struct ReceiverStats {
    log_records: AtomicU64,
    metric_points: AtomicU64,
    spans: AtomicU64,
    stopped: AtomicBool,
}

/// Everything the three tonic services need.
struct Shared {
    out: Arc<dyn EventOut>,
    mapper: Arc<PathMapper>,
    stats: Arc<ReceiverStats>,
}

impl Shared {
    fn emit_all(&self, events: Vec<Event>) {
        for event in events {
            self.out.emit(event);
        }
    }
}

struct LogsSvc(Arc<Shared>);
struct MetricsSvc(Arc<Shared>);
struct TraceSvc(Arc<Shared>);

#[tonic::async_trait]
impl LogsService for LogsSvc {
    async fn export(
        &self,
        request: tonic::Request<ExportLogsServiceRequest>,
    ) -> Result<tonic::Response<ExportLogsServiceResponse>, tonic::Status> {
        let request = request.into_inner();
        let records: usize = request
            .resource_logs
            .iter()
            .flat_map(|resource| &resource.scope_logs)
            .map(|scope| scope.log_records.len())
            .sum();
        let mut events = Vec::new();
        walk_logs(&request, &self.0.mapper, &mut events);
        self.0
            .stats
            .log_records
            .fetch_add(to_u64(records), Ordering::Relaxed);
        self.0.emit_all(events);
        // ALWAYS Ok, even when the bus is dropping everything. An error Status
        // makes the OTel exporter retry, which is backpressure with extra steps.
        Ok(tonic::Response::new(ExportLogsServiceResponse {
            partial_success: None,
        }))
    }
}

#[tonic::async_trait]
impl MetricsService for MetricsSvc {
    async fn export(
        &self,
        request: tonic::Request<ExportMetricsServiceRequest>,
    ) -> Result<tonic::Response<ExportMetricsServiceResponse>, tonic::Status> {
        let request = request.into_inner();
        let mut events = Vec::new();
        let appended = walk_metrics(&request, &mut events);
        self.0
            .stats
            .metric_points
            .fetch_add(to_u64(appended), Ordering::Relaxed);
        self.0.emit_all(events);
        Ok(tonic::Response::new(ExportMetricsServiceResponse {
            partial_success: None,
        }))
    }
}

#[tonic::async_trait]
impl TraceService for TraceSvc {
    async fn export(
        &self,
        request: tonic::Request<ExportTraceServiceRequest>,
    ) -> Result<tonic::Response<ExportTraceServiceResponse>, tonic::Status> {
        let request = request.into_inner();
        let spans: usize = request
            .resource_spans
            .iter()
            .flat_map(|resource| &resource.scope_spans)
            .map(|scope| scope.spans.len())
            .sum();
        let mut events = Vec::new();
        walk_traces(&request, &self.0.mapper, &mut events);
        self.0
            .stats
            .spans
            .fetch_add(to_u64(spans), Ordering::Relaxed);
        self.0.emit_all(events);
        Ok(tonic::Response::new(ExportTraceServiceResponse {
            partial_success: None,
        }))
    }
}

fn to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// The OTLP receiver and its private tokio runtime.
pub struct OtlpReceiver {
    local_addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    stats: Arc<ReceiverStats>,
    started: Instant,
}

impl fmt::Debug for OtlpReceiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtlpReceiver")
            .field("local_addr", &self.local_addr)
            .field("running", &self.thread.is_some())
            .field("spans_seen", &self.has_seen_spans())
            .finish_non_exhaustive()
    }
}

impl OtlpReceiver {
    /// Binds `addr` synchronously, then serves on a private two-worker runtime
    /// running on a named background thread.
    ///
    /// Two workers is ample: the handler is ~50 µs of pure decode and traffic is
    /// roughly one batch per second per agent.
    ///
    /// The returned error is the *bind* error and nothing else, so a caller can
    /// match [`std::io::ErrorKind::AddrInUse`] and degrade (ADR-0011). It is
    /// never a panic: binding happens on the caller's thread precisely so that
    /// the failure is a value rather than an unwind inside a background task
    /// nobody joins.
    pub fn start(
        addr: SocketAddr,
        sink: EventSink,
        mapper: PathMapper,
    ) -> Result<Self, std::io::Error> {
        Self::start_with_output(addr, Arc::new(sink), mapper)
    }

    fn start_with_output(
        addr: SocketAddr,
        out: Arc<dyn EventOut>,
        mapper: PathMapper,
    ) -> Result<Self, std::io::Error> {
        // Bind SYNCHRONOUSLY, here, on the caller's thread. This is the whole
        // trick (ADR-0011): WSAEADDRINUSE / EADDRINUSE surfaces as a plain
        // io::Error the caller can match, instead of a panic on a detached
        // runtime thread. Never match the message text — it is localised.
        let listener = std::net::TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let stats = Arc::new(ReceiverStats::default());
        let shared = Arc::new(Shared {
            out,
            mapper: Arc::new(mapper),
            stats: Arc::clone(&stats),
        });
        let exit_stats = Arc::clone(&stats);

        let thread = std::thread::Builder::new()
            .name("polis-otlp".to_owned())
            .spawn(move || {
                serve(listener, &shared, shutdown_rx);
                exit_stats.stopped.store(true, Ordering::Release);
            })?;

        Ok(Self {
            local_addr,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
            stats,
            started: Instant::now(),
        })
    }

    /// The address actually bound. Always the requested one — Polis never falls
    /// back to a different port — but returned so `polis doctor` can print it,
    /// and so a test can ask for an ephemeral one.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Whether any span has arrived on `TraceService` yet.
    ///
    /// The beta traces channel is the only source of subagent attribution
    /// (ADR-0006), and it can be silently off — the env block missing one
    /// variable, or a Claude Code release withdrawing the beta. A receiver that
    /// has seen logs but no spans is **degraded**, not healthy, and this is how
    /// that is detected.
    pub fn has_seen_spans(&self) -> bool {
        self.stats.spans.load(Ordering::Relaxed) > 0
    }

    /// Log records decoded since start. Cheap and non-blocking; for the status
    /// bar and `polis doctor`.
    pub fn log_records_seen(&self) -> u64 {
        self.stats.log_records.load(Ordering::Relaxed)
    }

    /// Metric events produced since start. Cheap and non-blocking.
    pub fn metric_points_seen(&self) -> u64 {
        self.stats.metric_points.load(Ordering::Relaxed)
    }

    /// Stops the runtime and joins its thread.
    ///
    /// `serve_with_incoming_shutdown` plus a `oneshot`, which is why `tokio`
    /// carries the `sync` feature, and then a hard [`SHUTDOWN_GRACE`] deadline
    /// so an agent's idle connection cannot hold Polis's exit open. Idempotent,
    /// and [`Drop`] calls it too, so a panic on the UI thread still unwinds into
    /// a clean socket close.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            // Err means the runtime already exited; nothing to do either way.
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            // Never propagate: shutdown runs while the process is already on its
            // way out, and a second panic there would abort. But never swallow
            // it silently either — a panicking serve thread hid a real shutdown
            // bug behind a passing test suite for exactly as long as this line
            // said nothing.
            if thread.join().is_err() {
                tracing::error!("the OTLP server thread panicked; see the panic message above");
            }
        }
    }
}

impl Drop for OtlpReceiver {
    fn drop(&mut self) {
        self.stop();
    }
}

impl IngestSource for OtlpReceiver {
    fn channel(&self) -> Channel {
        Channel::Otel
    }

    fn health(&self) -> SourceHealth {
        health_from(
            self.stats.log_records.load(Ordering::Relaxed),
            self.stats.spans.load(Ordering::Relaxed),
            self.started.elapsed(),
            self.stats.stopped.load(Ordering::Acquire),
        )
    }

    fn shutdown(self: Box<Self>) {
        (*self).shutdown();
    }
}

/// The health rule, factored out so it is testable without a socket.
///
/// Logs flowing with no spans after the grace window means the beta traces
/// channel is off and every tool call will be attributed to the main agent —
/// degraded, not healthy, and never silently guessed at (ADR-0006).
fn health_from(records: u64, spans: u64, elapsed: Duration, stopped: bool) -> SourceHealth {
    if stopped {
        return SourceHealth::Stopped {
            reason: "the OTLP server thread exited; Channel A is off".to_owned(),
        };
    }
    if records > 0 && spans == 0 && elapsed >= SPAN_GRACE {
        return SourceHealth::Degraded {
            reason: format!(
                "{records} log records but no claude_code spans in {}s: subagent attribution \
                 falls back to the main agent. Check CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1 and \
                 OTEL_TRACES_EXPORTER=otlp",
                elapsed.as_secs()
            ),
        };
    }
    SourceHealth::Running
}

fn serve(
    listener: std::net::TcpListener,
    shared: &Arc<Shared>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(RUNTIME_WORKER_THREADS)
        .thread_name("polis-otlp-rt")
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::warn!(%error, "OTLP runtime build failed; continuing without Channel A");
            return;
        }
    };

    // A second oneshot, so that the drain deadline below and tonic's graceful
    // shutdown are driven by the same signal without `tokio::select!` — which
    // lives behind the `macros` feature Polis deliberately does not enable.
    let (drain_tx, drain_rx) = tokio::sync::oneshot::channel::<()>();
    let services = Arc::clone(shared);

    let spawned = runtime.block_on(async move {
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let router = tonic::transport::Server::builder()
            .add_service(configure(LogsServiceServer::new(LogsSvc(Arc::clone(
                &services,
            )))))
            .add_service(configure(MetricsServiceServer::new(MetricsSvc(
                Arc::clone(&services),
            ))))
            .add_service(configure(TraceServiceServer::new(TraceSvc(services))));
        Ok::<_, std::io::Error>(tokio::spawn(async move {
            router
                .serve_with_incoming_shutdown(incoming, async move {
                    let _ = drain_rx.await;
                })
                .await
        }))
    });
    let server = match spawned {
        Ok(server) => server,
        Err(error) => {
            tracing::warn!(%error, "handing the OTLP listener to tokio failed");
            return;
        }
    };

    // Park until someone asks for a stop. `Err` means the handle was dropped
    // without calling `shutdown`, which is also a stop.
    runtime.block_on(async move {
        let _ = shutdown.await;
    });
    let _ = drain_tx.send(());

    // Bounded drain: a live agent's idle HTTP/2 connection would otherwise hold
    // graceful shutdown — and therefore Polis's exit — open indefinitely.
    //
    // `timeout` is constructed INSIDE the async block, not as an argument to
    // `block_on`. `tokio::time::timeout` builds its `Sleep` eagerly and a
    // `Sleep` needs the current runtime's timer driver, so evaluating it on the
    // caller's side of `block_on` panics with "there is no reactor running,
    // must be called from the context of a Tokio 1.x runtime" — on the
    // `polis-otlp` thread, at every shutdown. `stop()` joined with
    // `let _ = thread.join()`, so the panic was invisible to the unit tests and
    // only showed up as stray text under a real `polis tail`.
    match runtime.block_on(async move { tokio::time::timeout(SHUTDOWN_GRACE, server).await }) {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => tracing::warn!(%error, "OTLP server exited with an error"),
        Ok(Err(error)) => tracing::warn!(%error, "the OTLP server task did not finish cleanly"),
        Err(_) => {
            tracing::warn!(
                "OTLP connections did not drain within {SHUTDOWN_GRACE:?}; closing them"
            );
        }
    }
    // The runtime drops here, which drops the listener and any connection that
    // outlived the grace window.
}

/// The two settings all three services share: a 16 MiB decode ceiling and gzip
/// acceptance, in case `OTEL_EXPORTER_OTLP_COMPRESSION` is ever set.
fn configure<S: ConfigurableService>(server: S) -> S {
    server.apply()
}

/// Shim so the three generated `*ServiceServer<T>` types share one configuration
/// site. They have no common trait of their own, only three inherent methods
/// with the same names.
trait ConfigurableService: Sized {
    fn apply(self) -> Self;
}

macro_rules! configurable {
    ($($ty:ident),+ $(,)?) => {
        $(
            impl<T> ConfigurableService for $ty<T> {
                fn apply(self) -> Self {
                    self.max_decoding_message_size(MAX_DECODING_MESSAGE_SIZE)
                        .accept_compressed(tonic::codec::CompressionEncoding::Gzip)
                }
            }
        )+
    };
}

configurable!(LogsServiceServer, MetricsServiceServer, TraceServiceServer);

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use std::sync::Mutex;

    use opentelemetry_proto::tonic::common::v1::{
        any_value::Value, AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList,
    };
    use opentelemetry_proto::tonic::logs::v1::{ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::metrics::v1::{
        metric::Data, number_data_point, Gauge, Histogram, HistogramDataPoint, Metric,
        NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum,
    };
    use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};

    use polis_events::{
        ControlEvent, MetricName, Outcome, PromptId, SessionId, Temporality, ToolKind, ToolUseId,
        WorkerId, WorktreeId, OTEL_EVENT_NAMES, OTEL_SPAN_NAMES, PII_ATTRIBUTES,
        REFUSED_EVENT_NAMES,
    };

    const ROOT: &str = "C:/coding/agentolis";
    const SESSION: &str = "4a892b7b-1111-2222-3333-444455556666";

    // -- builders -----------------------------------------------------------

    fn mapper() -> PathMapper {
        PathMapper::new(Path::new(ROOT)).expect("the repo root must be an absolute path")
    }

    fn kv(key: &str, value: Value) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue { value: Some(value) }),
            ..KeyValue::default()
        }
    }

    fn text(value: &str) -> Value {
        Value::StringValue(value.to_owned())
    }

    fn claude_resource() -> Resource {
        Resource {
            attributes: vec![
                kv("service.name", text("claude-code")),
                kv("service.version", text("2.1.248")),
                kv("os.type", text("windows")),
                kv("os.version", text("10.0.26200")),
                kv("host.arch", text("amd64")),
            ],
            ..Resource::default()
        }
    }

    fn scope(name: &str) -> InstrumentationScope {
        InstrumentationScope {
            name: name.to_owned(),
            version: "2.1.248".to_owned(),
            ..InstrumentationScope::default()
        }
    }

    fn log_record(body: &str, attributes: Vec<KeyValue>) -> LogRecord {
        LogRecord {
            time_unix_nano: 1_788_284_877_527_324_100,
            observed_time_unix_nano: 1_788_284_877_527_324_100,
            // Always UNSPECIFIED / empty from Claude Code. A receiver that
            // filters on severity drops 100% of the traffic.
            severity_number: 0,
            severity_text: String::new(),
            body: Some(AnyValue {
                value: Some(text(body)),
            }),
            attributes,
            ..LogRecord::default()
        }
    }

    fn logs_request(records: Vec<LogRecord>) -> ExportLogsServiceRequest {
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(claude_resource()),
                scope_logs: vec![ScopeLogs {
                    // Undocumented and free to move: nothing dispatches on it.
                    scope: Some(scope("com.anthropic.claude_code.events")),
                    log_records: records,
                    ..ScopeLogs::default()
                }],
                ..ResourceLogs::default()
            }],
        }
    }

    fn traces_request(spans: Vec<Span>) -> ExportTraceServiceRequest {
        ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(claude_resource()),
                scope_spans: vec![ScopeSpans {
                    scope: Some(scope("com.anthropic.claude_code")),
                    spans,
                    ..ScopeSpans::default()
                }],
                ..ResourceSpans::default()
            }],
        }
    }

    fn metrics_request(metrics: Vec<Metric>) -> ExportMetricsServiceRequest {
        ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: Some(claude_resource()),
                scope_metrics: vec![ScopeMetrics {
                    scope: Some(scope("com.anthropic.claude_code")),
                    metrics,
                    ..ScopeMetrics::default()
                }],
                ..ResourceMetrics::default()
            }],
        }
    }

    /// The standard attributes every record and every data point carries,
    /// including the PII ones so that every test exercises the scrubbing.
    fn standard_attrs(sequence: i64) -> Vec<KeyValue> {
        vec![
            kv("session.id", text(SESSION)),
            kv("user.id", text("0f1e2d3c")),
            kv("user.email", text("operator@example.com")),
            kv(
                "user.account_uuid",
                text("cafebabe-0000-0000-0000-000000000000"),
            ),
            kv("user.account_id", text("user_01KFVLC")),
            kv(
                "organization.id",
                text("11111111-2222-3333-4444-555555555555"),
            ),
            kv("terminal.type", text("windows-terminal")),
            kv("event.timestamp", text("2026-09-01T17:40:38.740Z")),
            kv("event.sequence", Value::IntValue(sequence)),
        ]
    }

    fn number_point(value: f64, extra: Vec<KeyValue>) -> NumberDataPoint {
        let mut attributes = standard_attrs(0);
        attributes.extend(extra);
        NumberDataPoint {
            attributes,
            start_time_unix_nano: 1_788_285_009_551_000_000,
            time_unix_nano: 1_788_285_012_549_000_000,
            value: Some(number_data_point::Value::AsDouble(value)),
            ..NumberDataPoint::default()
        }
    }

    fn sum_metric(name: &str, temporality: i32, points: Vec<NumberDataPoint>) -> Metric {
        Metric {
            name: name.to_owned(),
            unit: "tokens".to_owned(),
            data: Some(Data::Sum(Sum {
                data_points: points,
                aggregation_temporality: temporality,
                is_monotonic: true,
            })),
            ..Metric::default()
        }
    }

    fn tool_span(agent_id: Option<&str>) -> Span {
        let mut attributes = standard_attrs(0);
        attributes.extend([
            kv("span.type", text("tool")),
            kv("tool_name", text("Read")),
            kv("tool_use_id", text("toolu_01WtV")),
            kv(
                "file_path",
                text("C:\\coding\\agentolis\\polis-events\\src\\lib.rs"),
            ),
            kv("success", text("true")),
            kv("duration_ms", Value::DoubleValue(2.0)),
        ]);
        if let Some(id) = agent_id {
            attributes.push(kv("agent_id", text(id)));
        }
        Span {
            trace_id: vec![0x4b; 16],
            span_id: vec![0x66, 0x00, 0x6a, 0x7d, 0xaf, 0x29, 0xb6, 0x5b],
            parent_span_id: vec![0x5c, 0xd4, 0x50, 0xb9, 0xb4, 0x34, 0x3a, 0x2f],
            name: "claude_code.tool".to_owned(),
            start_time_unix_nano: 1_000_000_000,
            end_time_unix_nano: 1_002_000_000,
            attributes,
            ..Span::default()
        }
    }

    // -- inspectors ---------------------------------------------------------

    fn otel_body(event: &Event) -> &OtelEvent {
        match &event.payload {
            Payload::Otel(inner) => inner,
            other => panic!("expected an Otel payload, got {other:?}"),
        }
    }

    fn telemetry(events: &[Event]) -> Vec<&Event> {
        events
            .iter()
            .filter(|e| !matches!(e.payload, Payload::Control(_)))
            .collect()
    }

    fn controls(events: &[Event]) -> Vec<&ControlEvent> {
        events
            .iter()
            .filter_map(|e| match &e.payload {
                Payload::Control(control) => Some(control),
                _ => None,
            })
            .collect()
    }

    #[derive(Debug, Default)]
    struct Collector {
        events: Mutex<Vec<Event>>,
    }

    impl Collector {
        fn take(&self) -> Vec<Event> {
            std::mem::take(&mut self.events.lock().expect("collector poisoned"))
        }
    }

    impl EventOut for Collector {
        fn emit(&self, event: Event) {
            self.events.lock().expect("collector poisoned").push(event);
        }
    }

    /// A sink that models the worst case PRD §4.5 permits: everything dropped.
    /// The handler must still return `Ok` promptly.
    #[derive(Debug, Default)]
    struct BlackHole;

    impl EventOut for BlackHole {
        fn emit(&self, _event: Event) {}
    }

    fn shared_with(out: Arc<dyn EventOut>) -> Arc<Shared> {
        Arc::new(Shared {
            out,
            mapper: Arc::new(mapper()),
            stats: Arc::new(ReceiverStats::default()),
        })
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime must build")
    }

    // -- attribute extraction ----------------------------------------------

    /// Every `AnyValue` arm has to survive the walk, including the two the wire
    /// is legally allowed to leave empty and the profiling-only `strindex`
    /// (tag 8) that must stay non-fatal outside the Profiling signal.
    #[test]
    fn every_any_value_variant_survives_flattening() {
        let attributes = vec![
            kv("string", text("claude.ai Google Calendar")),
            kv("bool_true", Value::BoolValue(true)),
            kv("bool_false", Value::BoolValue(false)),
            kv("int", Value::IntValue(17)),
            kv("negative", Value::IntValue(-1)),
            kv("double", Value::DoubleValue(0.003_1)),
            kv("nan", Value::DoubleValue(f64::NAN)),
            kv("infinite", Value::DoubleValue(f64::INFINITY)),
            kv("bytes", Value::BytesValue(vec![0xde, 0xad, 0xbe, 0xef])),
            kv("strindex", Value::StringValueStrindex(9)),
            // An AnyValue with no arm set, and a KeyValue with no value at all.
            KeyValue {
                key: "empty_any_value".to_owned(),
                value: Some(AnyValue { value: None }),
                ..KeyValue::default()
            },
            KeyValue {
                key: "absent_value".to_owned(),
                value: None,
                ..KeyValue::default()
            },
            // Nested kvlist: flattens to dotted keys.
            KeyValue {
                key: "nested".to_owned(),
                value: Some(AnyValue {
                    value: Some(Value::KvlistValue(KeyValueList {
                        values: vec![
                            kv("inner", text("x")),
                            kv("n", Value::IntValue(3)),
                            kv("flag", Value::BoolValue(true)),
                        ],
                    })),
                }),
                ..KeyValue::default()
            },
            // Array of scalars: stays ONE rendered pair, because splitting it
            // destroys the reading of a permission-suggestion list.
            KeyValue {
                key: "scalar_array".to_owned(),
                value: Some(AnyValue {
                    value: Some(Value::ArrayValue(ArrayValue {
                        values: vec![
                            AnyValue {
                                value: Some(text("Edit(src/**)")),
                            },
                            AnyValue {
                                value: Some(text("Write(src/**)")),
                            },
                        ],
                    })),
                }),
                ..KeyValue::default()
            },
            // Array of kvlists: rendered, not lost.
            KeyValue {
                key: "structured_array".to_owned(),
                value: Some(AnyValue {
                    value: Some(Value::ArrayValue(ArrayValue {
                        values: vec![AnyValue {
                            value: Some(Value::KvlistValue(KeyValueList {
                                values: vec![
                                    kv("line", Value::IntValue(17)),
                                    kv("op", text("replace")),
                                ],
                            })),
                        }],
                    })),
                }),
                ..KeyValue::default()
            },
        ];

        let mut flat = Vec::new();
        flatten_attributes(&attributes, &mut flat);

        // Nothing may vanish: a decoder that matches one variant per key loses
        // fields with no error and no log line (ADR-0015).
        for key in [
            "string",
            "bool_true",
            "bool_false",
            "int",
            "negative",
            "double",
            "nan",
            "infinite",
            "bytes",
            "strindex",
            "empty_any_value",
            "absent_value",
            "scalar_array",
            "structured_array",
        ] {
            assert!(attr(&flat, key).is_some(), "{key} was lost in flattening");
        }
        assert_eq!(attr(&flat, "string"), Some("claude.ai Google Calendar"));
        assert_eq!(attr(&flat, "bool_true"), Some("true"));
        assert_eq!(attr(&flat, "int"), Some("17"));
        // Nesting becomes dotted keys, and the parent key itself disappears.
        assert_eq!(attr(&flat, "nested.inner"), Some("x"));
        assert_eq!(attr(&flat, "nested.n"), Some("3"));
        assert_eq!(attr(&flat, "nested.flag"), Some("true"));
        assert_eq!(attr(&flat, "nested"), None);
        // Both legally-empty spellings render as the empty string, not an error.
        assert_eq!(attr(&flat, "empty_any_value"), Some(""));
        assert_eq!(attr(&flat, "absent_value"), Some(""));
        // A scalar array keeps its elements distinguishable inside one value.
        let array = attr(&flat, "scalar_array").expect("kept");
        assert!(array.contains("Edit(src/**)") && array.contains("Write(src/**)"));
        let structured = attr(&flat, "structured_array").expect("kept");
        assert!(structured.contains("line") && structured.contains("17"));
    }

    #[test]
    fn flattening_drops_every_pii_attribute_by_name() {
        let mut attributes = standard_attrs(1);
        // …including one hidden inside a nested kvlist, which flattens to the
        // same dotted key and must be caught by the same rule.
        attributes.push(KeyValue {
            key: "user".to_owned(),
            value: Some(AnyValue {
                value: Some(Value::KvlistValue(KeyValueList {
                    values: vec![kv("email", text("leak@example.com"))],
                })),
            }),
            ..KeyValue::default()
        });
        let mut flat = Vec::new();
        flatten_attributes(&attributes, &mut flat);

        for pii in PII_ATTRIBUTES {
            assert_eq!(
                attr(&flat, pii),
                None,
                "{pii} must never survive ingest (ADR-0005)"
            );
        }
        let rendered = format!("{flat:?}");
        assert!(
            !rendered.contains("konkaiser"),
            "the email leaked: {rendered}"
        );
        assert!(!rendered.contains("leak@example.com"), "{rendered}");
        // A stable, non-identifying id is kept on purpose: Polis needs an agent
        // identity, not an identity document.
        assert_eq!(attr(&flat, "user.id"), Some("0f1e2d3c"));
        assert_eq!(attr(&flat, "session.id"), Some(SESSION));
    }

    // -- log records --------------------------------------------------------

    #[test]
    fn a_realistic_tool_result_normalises_to_a_tool_call_event() {
        let mut attributes = standard_attrs(12);
        attributes.extend([
            kv("event.name", text("tool_result")),
            kv("prompt.id", text("09a624c0-ca45-4e72-aeef-0ce917a44d18")),
            kv("tool_name", text("Edit")),
            kv("tool_use_id", text("toolu_01XZwEveLpHVQ361BFSPFwPV")),
            // `success` is the STRING "true" on this channel, not a bool.
            kv("success", text("true")),
            // `duration_ms` is an int here and a string on other events.
            kv("duration_ms", Value::IntValue(3)),
            kv("tool_input_size_bytes", Value::IntValue(153)),
            kv(
                "tool_input",
                text(
                    r#"{"file_path":"C:\\coding\\agentolis\\polis-ingest\\src\\otlp.rs","old_string":"fn a()","new_string":"fn b()"}"#,
                ),
            ),
        ]);
        let request = logs_request(vec![log_record("claude_code.tool_result", attributes)]);

        let mut events = Vec::new();
        assert_eq!(walk_logs(&request, &mapper(), &mut events), 1);
        let event = &events[0];

        assert_eq!(event.meta.channel, Channel::Otel);
        assert_eq!(
            event.meta.session.as_ref().map(SessionId::as_str),
            Some(SESSION)
        );
        assert!(
            event.meta.thread.is_some(),
            "the thread is derived from the session"
        );
        assert_eq!(
            event.meta.prompt.as_ref().map(PromptId::as_str),
            Some("09a624c0-ca45-4e72-aeef-0ce917a44d18")
        );
        assert_eq!(event.meta.sequence, Some(12));
        // `agent_id` occurs zero times on the logs channel: that means MAIN
        // AGENT, not a parse failure.
        assert!(event.meta.worker.is_none());

        match otel_body(event) {
            OtelEvent::ToolResult(call) => {
                assert_eq!(call.tool, ToolKind::Edit);
                assert_eq!(
                    call.tool_use_id.as_ref().map(ToolUseId::as_str),
                    Some("toolu_01XZwEveLpHVQ361BFSPFwPV")
                );
                assert_eq!(call.outcome, Outcome::Done);
                assert_eq!(call.duration_ms, Some(3.0));
                // The path came out of the JSON string with single separators; a
                // string-slicing decoder yields `C:\\coding\\…` here.
                assert_eq!(call.paths.len(), 1);
                assert_eq!(call.paths[0].0, WorktreeId::PRIMARY);
                assert_eq!(call.paths[0].1.as_str(), "polis-ingest/src/otlp.rs");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }

        // ADR-0005: extract, then discard. The raw JSON — which for a Write
        // holds the file's contents and for an Edit the diff — must not survive
        // into the event, and neither may the email on every record.
        let serialised = format!("{event:?}");
        assert!(!serialised.contains("old_string"), "{serialised}");
        assert!(!serialised.contains("konkaiser"), "{serialised}");
    }

    #[test]
    fn startup_records_without_a_prompt_id_are_normal_not_errors() {
        let attributes = vec![
            kv("session.id", text("s-1")),
            kv("event.name", text("plugin_loaded")),
            kv("plugin.name", text("playwright")),
            // A real bool sitting next to a stringly-typed one, in one event.
            kv("has_hooks", Value::BoolValue(true)),
            kv("safe_mode", text("false")),
        ];
        let request = logs_request(vec![log_record("claude_code.plugin_loaded", attributes)]);
        let mut events = Vec::new();
        assert_eq!(walk_logs(&request, &mapper(), &mut events), 1);
        assert!(
            events[0].meta.prompt.is_none(),
            "startup traffic legitimately has no prompt.id"
        );
        assert!(events[0].meta.sequence.is_none());
        assert!(
            matches!(otel_body(&events[0]), OtelEvent::PluginLoaded { .. }),
            "got {:?}",
            otel_body(&events[0])
        );
        assert!(
            controls(&events).is_empty(),
            "a modelled event name is not drift"
        );
    }

    #[test]
    fn an_unknown_event_name_becomes_drift_and_never_a_failure() {
        let request = logs_request(vec![
            log_record("claude_code.brand_new_in_2_1_260", standard_attrs(1)),
            log_record("claude_code.also_new", standard_attrs(2)),
            log_record("claude_code.brand_new_in_2_1_260", standard_attrs(3)),
        ]);
        let mut events = Vec::new();
        walk_logs(&request, &mapper(), &mut events);

        // The records survive as Unknown — a silently dropped record is what
        // PRD §17's drift warning exists to prevent — and the batch carries one
        // report naming both new spellings.
        let kept = telemetry(&events);
        assert_eq!(kept.len(), 3);
        assert!(kept
            .iter()
            .all(|e| matches!(otel_body(e), OtelEvent::Unknown { .. })));

        let reports = controls(&events);
        assert_eq!(
            reports.len(),
            1,
            "one drift report per batch, not one per record"
        );
        match reports[0] {
            ControlEvent::SchemaDrift {
                channel,
                producer_version,
                detail,
            } => {
                assert_eq!(*channel, Channel::Otel);
                // Every drift report carries the release that produced it.
                assert_eq!(producer_version.as_deref(), Some("2.1.248"));
                assert!(detail.contains("brand_new_in_2_1_260"), "{detail}");
                assert!(detail.contains("also_new"), "{detail}");
            }
            other => panic!("expected SchemaDrift, got {other:?}"),
        }
    }

    #[test]
    fn raw_api_bodies_are_refused_by_name_and_produce_nothing_at_all() {
        for refused in REFUSED_EVENT_NAMES {
            let attributes = vec![
                kv("session.id", text("s-1")),
                kv("event.name", text(refused)),
                kv(
                    "body",
                    text(r#"{"messages":[{"role":"user","content":"my whole life story"}]}"#),
                ),
            ];
            let request = logs_request(vec![log_record(
                &format!("claude_code.{refused}"),
                attributes,
            )]);
            let mut events = Vec::new();
            walk_logs(&request, &mapper(), &mut events);
            assert!(
                events.is_empty(),
                "{refused} must produce no event at all, not even drift"
            );
        }
        // …and the refusal is a positive match against a name that really is in
        // the dispatch table, not an accident of omission (ADR-0005).
        for refused in REFUSED_EVENT_NAMES {
            assert!(OTEL_EVENT_NAMES.contains(refused));
        }
    }

    #[test]
    fn foreign_otlp_traffic_on_the_unauthenticated_port_is_dropped_whole() {
        let foreign = || Resource {
            attributes: vec![kv("service.name", text("some-other-app"))],
            ..Resource::default()
        };

        let mut request = logs_request(vec![log_record(
            "claude_code.tool_result",
            standard_attrs(1),
        )]);
        request.resource_logs[0].resource = Some(foreign());
        let mut events = Vec::new();
        assert_eq!(walk_logs(&request, &mapper(), &mut events), 0);

        // Metrics and spans get the same guard — `otlp_metric` and `otlp_span`
        // never see the resource block, so it can only happen here.
        let mut metrics = metrics_request(vec![sum_metric(
            "claude_code.token.usage",
            1,
            vec![number_point(1.0, vec![])],
        )]);
        metrics.resource_metrics[0].resource = Some(foreign());
        events.clear();
        assert_eq!(walk_metrics(&metrics, &mut events), 0);

        let mut traces = traces_request(vec![tool_span(None)]);
        traces.resource_spans[0].resource = Some(foreign());
        events.clear();
        assert_eq!(walk_traces(&traces, &mapper(), &mut events), 0);
    }

    #[test]
    fn a_missing_resource_block_degrades_rather_than_blacking_out() {
        // An absent `service.name` is not proof of foreign traffic, and an empty
        // city that looks healthy is the worst outcome there is (ADR-0011).
        let mut request = logs_request(vec![log_record(
            "claude_code.tool_result",
            standard_attrs(1),
        )]);
        request.resource_logs[0].resource = None;
        let mut events = Vec::new();
        assert_eq!(walk_logs(&request, &mapper(), &mut events), 1);
    }

    #[test]
    fn the_event_name_attribute_backs_up_an_empty_body() {
        let mut record = log_record("", standard_attrs(4));
        record.body = None;
        record
            .attributes
            .push(kv("event.name", text("user_prompt")));
        // `prompt_length` is a STRING on the wire despite being a count.
        record.attributes.push(kv("prompt_length", text("490")));
        let request = logs_request(vec![record]);
        let mut events = Vec::new();
        assert_eq!(walk_logs(&request, &mapper(), &mut events), 1);
        match otel_body(&events[0]) {
            OtelEvent::UserPrompt { length } => assert_eq!(*length, Some(490)),
            other => panic!("expected UserPrompt, got {other:?}"),
        }
    }

    #[test]
    fn the_prefix_is_stripped_for_every_modelled_event_name() {
        // The body is fully qualified; OTEL_EVENT_NAMES is not. A receiver that
        // forgets the strip matches nothing at all.
        for name in OTEL_EVENT_NAMES {
            if REFUSED_EVENT_NAMES.contains(name) {
                continue;
            }
            let request = logs_request(vec![log_record(
                &format!("{CLAUDE_PREFIX}{name}"),
                standard_attrs(1),
            )]);
            let mut events = Vec::new();
            assert_eq!(walk_logs(&request, &mapper(), &mut events), 1, "{name}");
            assert!(
                !matches!(otel_body(&events[0]), OtelEvent::Unknown { .. }),
                "{name} is in OTEL_EVENT_NAMES but decoded to Unknown"
            );
            assert!(controls(&events).is_empty(), "{name} was reported as drift");
        }
    }

    // -- spans --------------------------------------------------------------

    #[test]
    fn a_tool_span_carries_the_only_subagent_discriminator_there_is() {
        let request = traces_request(vec![tool_span(Some("a106e5fe86fce476a"))]);
        let mut events = Vec::new();
        assert_eq!(walk_traces(&request, &mapper(), &mut events), 1);
        let event = &events[0];

        assert_eq!(
            event.meta.worker.as_ref().map(WorkerId::as_str),
            Some("a106e5fe86fce476a")
        );
        assert!(event.meta.is_worker());
        match otel_body(event) {
            OtelEvent::ToolSpan {
                call,
                span_id,
                parent_span_id,
            } => {
                assert_eq!(call.tool, ToolKind::Read);
                assert_eq!(call.outcome, Outcome::Done);
                assert_eq!(call.duration_ms, Some(2.0));
                // The span's `file_path` is a clean top-level attribute: this
                // route needs no JSON parsing at all.
                assert_eq!(call.paths.len(), 1);
                assert_eq!(call.paths[0].1.as_str(), "polis-events/src/lib.rs");
                // Raw bytes on the wire; hex here, so the ids join to the log
                // records that enabling tracing retro-fits them onto.
                assert_eq!(span_id.as_deref(), Some("66006a7daf29b65b"));
                assert_eq!(parent_span_id.as_deref(), Some("5cd450b9b4343a2f"));
            }
            other => panic!("expected ToolSpan, got {other:?}"),
        }
    }

    #[test]
    fn an_absent_agent_id_means_main_agent_not_a_parse_failure() {
        let request = traces_request(vec![tool_span(None)]);
        let mut events = Vec::new();
        assert_eq!(walk_traces(&request, &mapper(), &mut events), 1);
        assert!(events[0].meta.worker.is_none());
        assert!(!events[0].meta.is_worker());
    }

    #[test]
    fn all_five_modelled_spans_normalise_and_the_hook_span_stays_unmodelled() {
        let mut spans = Vec::new();
        for name in OTEL_SPAN_NAMES {
            let mut attributes = standard_attrs(0);
            attributes.extend([
                kv("tool_use_id", text("toolu_x")),
                kv("model", text("claude-opus-5")),
                kv("llm_request.context", text("standalone")),
                kv("interaction.sequence", Value::IntValue(2)),
                kv("interaction.duration_ms", Value::DoubleValue(1234.5)),
                kv("user_prompt_length", text("490")),
                kv("success", text("true")),
            ]);
            spans.push(Span {
                span_id: vec![1, 2, 3, 4, 5, 6, 7, 8],
                trace_id: vec![9; 16],
                name: (*name).to_owned(),
                start_time_unix_nano: 10,
                end_time_unix_nano: 10 + 3_000_000,
                attributes,
                ..Span::default()
            });
        }
        let mut events = Vec::new();
        let appended = walk_traces(&traces_request(spans), &mapper(), &mut events);

        assert_eq!(
            appended,
            OTEL_SPAN_NAMES.len() - 1,
            "every span name but claude_code.hook is modelled (ADR-0045)"
        );
        assert!(
            controls(&events).is_empty(),
            "a name that is in the table is not drift"
        );
        for event in &events {
            assert!(
                !matches!(otel_body(event), OtelEvent::Unknown { .. }),
                "a modelled span decoded to Unknown: {event:?}"
            );
        }

        let interaction = events
            .iter()
            .find(|e| matches!(otel_body(e), OtelEvent::InteractionSpan { .. }))
            .expect("the interaction span must be there");
        match otel_body(interaction) {
            OtelEvent::InteractionSpan {
                sequence,
                duration_ms,
                prompt_length,
                ..
            } => {
                assert_eq!(*sequence, Some(2));
                assert_eq!(*duration_ms, Some(1234.5));
                assert_eq!(*prompt_length, Some(490));
            }
            other => panic!("expected InteractionSpan, got {other:?}"),
        }

        // `blocked_on_user` carries no `duration_ms` attribute here, so the
        // walk's wall bracket (3 ms) has to supply it. It is the only direct
        // measurement of PRD §11.2 attention state (a) there is.
        let blocked = events
            .iter()
            .find(|e| matches!(otel_body(e), OtelEvent::ToolBlockedOnUserSpan { .. }))
            .expect("the blocked_on_user span must be there");
        match otel_body(blocked) {
            OtelEvent::ToolBlockedOnUserSpan { duration_ms, .. } => {
                assert_eq!(*duration_ms, Some(3.0));
            }
            other => panic!("expected ToolBlockedOnUserSpan, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_span_name_becomes_drift_and_never_a_failure() {
        let span = Span {
            name: "claude_code.something_new".to_owned(),
            span_id: vec![1; 8],
            trace_id: vec![2; 16],
            attributes: standard_attrs(0),
            ..Span::default()
        };
        let mut events = Vec::new();
        walk_traces(&traces_request(vec![span]), &mapper(), &mut events);
        assert_eq!(controls(&events).len(), 1);
        assert!(matches!(
            controls(&events)[0],
            ControlEvent::SchemaDrift { .. }
        ));
        assert!(matches!(
            otel_body(telemetry(&events)[0]),
            OtelEvent::Unknown { .. }
        ));
    }

    // -- metrics ------------------------------------------------------------

    #[test]
    fn temporality_is_read_off_the_wire_and_never_assumed() {
        // 1 = DELTA, which is what Claude Code sends for all eight counters.
        let delta = sum_metric(
            "claude_code.token.usage",
            1,
            vec![number_point(17_628.0, vec![kv("type", text("cacheRead"))])],
        );
        let mut events = Vec::new();
        assert_eq!(walk_metrics(&metrics_request(vec![delta]), &mut events), 1);
        match &events[0].payload {
            Payload::Metric(metric) => {
                assert_eq!(metric.name, MetricName::TokenUsage);
                assert_eq!(metric.temporality, Temporality::Delta);
                assert!((metric.value - 17_628.0).abs() < f64::EPSILON);
                assert_eq!(
                    metric
                        .attributes
                        .get("type")
                        .and_then(serde_json::Value::as_str),
                    Some("cacheRead")
                );
            }
            other => panic!("expected a Metric payload, got {other:?}"),
        }

        // 2 = CUMULATIVE, reachable through a managed-settings override of
        // OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE.
        let cumulative = sum_metric(
            "claude_code.cost.usage",
            2,
            vec![number_point(0.018_298_8, vec![])],
        );
        events.clear();
        assert_eq!(
            walk_metrics(&metrics_request(vec![cumulative]), &mut events),
            1
        );
        match &events[0].payload {
            Payload::Metric(metric) => {
                assert_eq!(metric.name, MetricName::CostUsage);
                assert_eq!(metric.temporality, Temporality::Cumulative);
            }
            other => panic!("expected a Metric payload, got {other:?}"),
        }

        // 0 = UNSPECIFIED: kept, and reported as drift rather than guessed at.
        let unspecified = sum_metric(
            "claude_code.commit.count",
            0,
            vec![number_point(1.0, vec![])],
        );
        events.clear();
        walk_metrics(&metrics_request(vec![unspecified]), &mut events);
        assert_eq!(controls(&events).len(), 1);
        match &telemetry(&events)[0].payload {
            Payload::Metric(metric) => assert_eq!(metric.temporality, Temporality::Unspecified),
            other => panic!("expected a Metric payload, got {other:?}"),
        }
    }

    #[test]
    fn all_eight_counter_names_map_and_anything_else_degrades() {
        let expected = [
            ("claude_code.session.count", MetricName::SessionCount),
            ("claude_code.lines_of_code.count", MetricName::LinesOfCode),
            (
                "claude_code.pull_request.count",
                MetricName::PullRequestCount,
            ),
            ("claude_code.commit.count", MetricName::CommitCount),
            ("claude_code.cost.usage", MetricName::CostUsage),
            ("claude_code.token.usage", MetricName::TokenUsage),
            (
                "claude_code.code_edit_tool.decision",
                MetricName::CodeEditToolDecision,
            ),
            // A counter despite the `.total` name.
            ("claude_code.active_time.total", MetricName::ActiveTime),
        ];
        for (wire, name) in expected {
            let mut events = Vec::new();
            let request =
                metrics_request(vec![sum_metric(wire, 1, vec![number_point(1.0, vec![])])]);
            assert_eq!(walk_metrics(&request, &mut events), 1, "{wire}");
            match &events[0].payload {
                Payload::Metric(metric) => assert_eq!(metric.name, name, "{wire}"),
                other => panic!("expected a Metric payload, got {other:?}"),
            }
        }

        let mut events = Vec::new();
        let request = metrics_request(vec![sum_metric(
            "claude_code.brand_new.count",
            1,
            vec![number_point(1.0, vec![])],
        )]);
        walk_metrics(&request, &mut events);
        match &events[0].payload {
            Payload::Metric(metric) => assert!(
                matches!(metric.name, MetricName::Other(_)),
                "{:?}",
                metric.name
            ),
            other => panic!("expected a Metric payload, got {other:?}"),
        }
    }

    #[test]
    fn gauges_histograms_and_empty_metrics_are_defensive_never_fatal() {
        // A Gauge carries no temporality field at all, which is itself worth
        // reporting rather than silently calling delta.
        let gauge = Metric {
            name: "claude_code.some_gauge".to_owned(),
            data: Some(Data::Gauge(Gauge {
                data_points: vec![number_point(5.0, vec![])],
            })),
            ..Metric::default()
        };
        let mut events = Vec::new();
        walk_metrics(&metrics_request(vec![gauge]), &mut events);
        assert_eq!(telemetry(&events).len(), 1);
        assert_eq!(controls(&events).len(), 1);

        // No histogram has ever been observed on this channel; the requirement
        // is only that one cannot take the receiver down.
        let histogram = Metric {
            name: "claude_code.some_histogram".to_owned(),
            data: Some(Data::Histogram(Histogram {
                data_points: vec![HistogramDataPoint {
                    attributes: standard_attrs(0),
                    count: 4,
                    sum: Some(12.5),
                    ..HistogramDataPoint::default()
                }],
                aggregation_temporality: 1,
            })),
            ..Metric::default()
        };
        events.clear();
        walk_metrics(&metrics_request(vec![histogram]), &mut events);

        // `metric::Data` is an Option, so `None` is reachable whenever a
        // producer sends an instrument this build does not know. Never unwrap.
        let empty = Metric {
            name: "claude_code.nothing".to_owned(),
            data: None,
            ..Metric::default()
        };
        events.clear();
        assert_eq!(walk_metrics(&metrics_request(vec![empty]), &mut events), 0);
    }

    #[test]
    fn metric_datapoints_carry_the_session_and_no_pii() {
        let request = metrics_request(vec![sum_metric(
            "claude_code.token.usage",
            1,
            vec![number_point(
                1.0,
                vec![
                    kv("type", text("input")),
                    // The metric side carries the [1m] context suffix the event
                    // side does not: joining on `model` needs normalising.
                    kv("model", text("claude-opus-5[1m]")),
                ],
            )],
        )]);
        let mut events = Vec::new();
        assert_eq!(walk_metrics(&request, &mut events), 1);
        match &events[0].payload {
            Payload::Metric(metric) => {
                for pii in PII_ATTRIBUTES {
                    assert!(
                        !metric.attributes.contains_key(*pii),
                        "{pii} reached a metric datapoint"
                    );
                }
                assert!(!format!("{metric:?}").contains("konkaiser"));
            }
            other => panic!("expected a Metric payload, got {other:?}"),
        }
        // `session.id` is on every data point and is the only agent key metrics
        // have — it is NOT a resource attribute.
        assert_eq!(
            events[0].meta.session.as_ref().map(SessionId::as_str),
            Some(SESSION)
        );
        assert!(
            events[0].meta.prompt.is_none(),
            "prompt.id is deliberately never on metrics"
        );
    }

    // -- transport ----------------------------------------------------------

    #[test]
    fn a_port_already_in_use_returns_err_and_never_panics() {
        let squatter = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a squatter");
        let addr = squatter.local_addr().expect("squatter addr");

        let result = OtlpReceiver::start_with_output(addr, Arc::new(BlackHole), mapper());
        let error = result.expect_err("a bound port must be an Err, not a panic");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::AddrInUse,
            // The OS message is localised — German on the recon machine — so
            // the kind is the only thing that may ever be matched (ADR-0011).
            "matched on ErrorKind, never on the localised message: {error}"
        );
        drop(squatter);
    }

    #[test]
    fn the_handlers_always_return_ok_even_when_every_event_is_dropped() {
        let shared = shared_with(Arc::new(BlackHole));
        let runtime = runtime();

        let logs = LogsSvc(Arc::clone(&shared));
        let request = logs_request(vec![log_record(
            "claude_code.tool_result",
            standard_attrs(1),
        )]);
        for _ in 0..64 {
            let response = runtime.block_on(logs.export(tonic::Request::new(request.clone())));
            assert!(
                response.is_ok(),
                "an error Status makes the exporter retry, which is backpressure with extra steps"
            );
        }

        let metrics = MetricsSvc(Arc::clone(&shared));
        let response =
            runtime.block_on(metrics.export(tonic::Request::new(metrics_request(vec![
                sum_metric(
                    "claude_code.session.count",
                    1,
                    vec![number_point(1.0, vec![])],
                ),
            ]))));
        assert!(response.is_ok());

        let traces = TraceSvc(Arc::clone(&shared));
        let response = runtime
            .block_on(traces.export(tonic::Request::new(traces_request(vec![tool_span(None)]))));
        assert!(response.is_ok());

        assert_eq!(shared.stats.log_records.load(Ordering::Relaxed), 64);
        assert_eq!(shared.stats.metric_points.load(Ordering::Relaxed), 1);
        assert_eq!(shared.stats.spans.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_hostile_payload_cannot_panic_a_handler() {
        let collector = Arc::new(Collector::default());
        let shared = shared_with(collector.clone());
        let runtime = runtime();
        let logs = LogsSvc(Arc::clone(&shared));

        let mut deep = AnyValue {
            value: Some(text("bottom")),
        };
        for _ in 0..64 {
            deep = AnyValue {
                value: Some(Value::KvlistValue(KeyValueList {
                    values: vec![KeyValue {
                        key: "n".to_owned(),
                        value: Some(deep),
                        ..KeyValue::default()
                    }],
                })),
            };
        }
        let hostile = vec![
            KeyValue {
                key: "deep".to_owned(),
                value: Some(deep),
                ..KeyValue::default()
            },
            KeyValue {
                key: String::new(),
                value: None,
                ..KeyValue::default()
            },
            // Valid JSON whose value the per-value budget cut: a truncated path
            // is a *wrong* path, not a short one.
            kv(
                "tool_input",
                text("{\"path\":\"C:\\\\coding\\\\agentolis\\\\very\u{2026}[713 chars]\"}"),
            ),
            // …and JSON that is not JSON at all.
            kv("tool_parameters", text("{\"file_path\": \u{FFFD}}")),
            kv("event.sequence", text("not a number")),
            kv("duration_ms", Value::DoubleValue(f64::NAN)),
        ];
        let request = logs_request(vec![
            log_record("claude_code.tool_result", hostile),
            log_record("", vec![]),
            log_record("not_even_prefixed", vec![]),
            log_record(&format!("claude_code.{}", "x".repeat(4096)), vec![]),
        ]);
        let response = runtime.block_on(logs.export(tonic::Request::new(request)));
        assert!(
            response.is_ok(),
            "a hostile batch is still a successful RPC"
        );

        let events = collector.take();
        assert!(!events.is_empty());
        // The drift report has to stay bounded however long the hostile name is.
        for control in controls(&events) {
            match control {
                ControlEvent::SchemaDrift { detail, .. } => assert!(
                    detail.chars().count() < 1024,
                    "a hostile name reached the status bar verbatim: {detail}"
                ),
                other => panic!("expected SchemaDrift, got {other:?}"),
            }
        }
        // The tool_result still decoded; it simply has nothing usable in it.
        let call = telemetry(&events)
            .into_iter()
            .find_map(|e| match otel_body(e) {
                OtelEvent::ToolResult(call) => Some(call),
                _ => None,
            })
            .expect("the tool_result record still decodes");
        assert!(
            call.paths.is_empty(),
            "neither malformed JSON nor a truncated value may yield a path"
        );
    }

    #[test]
    fn health_degrades_when_logs_arrive_but_the_beta_span_channel_is_off() {
        // Nothing has happened yet: not enough information to complain.
        assert_eq!(
            health_from(0, 0, Duration::from_secs(600), false),
            SourceHealth::Running
        );
        // Logs are flowing and the grace window has not elapsed.
        assert_eq!(
            health_from(50, 0, Duration::from_secs(1), false),
            SourceHealth::Running
        );
        // Spans arrived: attribution is sound.
        assert_eq!(
            health_from(50, 3, Duration::from_secs(600), false),
            SourceHealth::Running
        );
        // Logs, no spans, past the grace window: subagent attribution is blind,
        // and the operator is told which two env vars to check (ADR-0006).
        let degraded = health_from(50, 0, SPAN_GRACE, false);
        assert!(!degraded.is_healthy());
        let reason = degraded.reason().expect("a degraded channel states why");
        assert!(
            reason.contains("CLAUDE_CODE_ENHANCED_TELEMETRY_BETA"),
            "{reason}"
        );
        assert!(reason.contains("OTEL_TRACES_EXPORTER"), "{reason}");
        // A dead thread is Stopped, not Degraded.
        assert!(matches!(
            health_from(0, 0, Duration::ZERO, true),
            SourceHealth::Stopped { .. }
        ));
    }

    #[test]
    fn the_receiver_serves_all_three_signals_over_a_real_socket_and_shuts_down() {
        use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
        use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_client::MetricsServiceClient;
        use opentelemetry_proto::tonic::collector::trace::v1::trace_service_client::TraceServiceClient;

        let collector = Arc::new(Collector::default());
        let receiver = OtlpReceiver::start_with_output(
            "127.0.0.1:0".parse().expect("a literal loopback address"),
            collector.clone(),
            mapper(),
        )
        .expect("binding an ephemeral loopback port must succeed");

        // Loopback only: this stream carries file paths and, before scrubbing, a
        // real email address. Never 0.0.0.0.
        assert!(receiver.local_addr().ip().is_loopback());
        assert!(!receiver.has_seen_spans());
        assert!(receiver.health().is_healthy());

        let endpoint = format!("http://{}", receiver.local_addr());
        let runtime = runtime();
        runtime.block_on(async {
            let work = async {
                let mut logs = LogsServiceClient::connect(endpoint.clone())
                    .await
                    .expect("connect to the logs service");
                logs.export(logs_request(vec![log_record(
                    "claude_code.tool_result",
                    standard_attrs(1),
                )]))
                .await
                .expect("the logs handler must always answer Ok");

                let mut metrics = MetricsServiceClient::connect(endpoint.clone())
                    .await
                    .expect("connect to the metrics service");
                metrics
                    .export(metrics_request(vec![sum_metric(
                        "claude_code.session.count",
                        1,
                        vec![number_point(1.0, vec![kv("start_type", text("fresh"))])],
                    )]))
                    .await
                    .expect("the metrics handler must always answer Ok");

                // The third service is not optional: without it Claude Code's
                // trace exports fail UNIMPLEMENTED and Polis loses its thread
                // model entirely (ADR-0006).
                let mut traces = TraceServiceClient::connect(endpoint.clone())
                    .await
                    .expect("connect to the trace service");
                traces
                    .export(traces_request(vec![tool_span(Some("a106e5fe86fce476a"))]))
                    .await
                    .expect("the trace handler must always answer Ok");
            };
            tokio::time::timeout(Duration::from_secs(30), work)
                .await
                .expect("the whole exchange must finish well inside 30s");
        });

        assert!(receiver.has_seen_spans(), "TraceService must be registered");
        assert_eq!(receiver.log_records_seen(), 1);
        assert_eq!(receiver.metric_points_seen(), 1);

        let events = collector.take();
        assert_eq!(events.len(), 3, "one per signal: {events:?}");
        assert!(events
            .iter()
            .any(|e| matches!(&e.payload, Payload::Otel(body)
            if matches!(**body, OtelEvent::ToolResult(_)))));
        assert!(events
            .iter()
            .any(|e| matches!(e.payload, Payload::Metric(_))));
        assert!(
            events.iter().any(|e| e.meta.is_worker()),
            "the span's agent_id is the whole reason TraceService is registered"
        );

        // The three client connections are still open here on purpose: a live
        // agent holds one for its whole session. `shutdown` must return anyway,
        // inside SHUTDOWN_GRACE, and this test hanging forever is exactly the
        // failure mode the bounded drain exists to prevent.
        //
        // `stopped` is stored by the serve thread only when `serve` RETURNS —
        // an unwind skips it — so this outlives the receiver on purpose. Without
        // it, a shutdown that panics its way out is indistinguishable from a
        // shutdown that drained: both are fast and both free the port, because
        // the unwind drops the runtime too. That is how a panic on every exit
        // survived a green suite until a real `polis tail` printed it.
        let exit = Arc::clone(&receiver.stats);
        let started = Instant::now();
        receiver.shutdown();
        assert!(
            started.elapsed() < SHUTDOWN_GRACE * 4,
            "shutdown took {:?}; it must be bounded",
            started.elapsed()
        );
        assert!(
            exit.stopped.load(Ordering::Acquire),
            "the serve thread must run to completion, not unwind out of it"
        );
        drop(runtime);
    }
}
