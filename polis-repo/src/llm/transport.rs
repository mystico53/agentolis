//! Bytes over a socket, and the one dependency decision in this feature.
//!
//! # Why there is no HTTP crate in `polis-repo`
//!
//! The brief asked for a light HTTP dependency, justified. The justification is
//! that the lightest one available here is **none**, and that the alternative is
//! heavier than it looks:
//!
//! * A blocking client with TLS (`ureq` + `rustls`) is roughly forty crates,
//!   including a cryptography library with a C build step and a **compiled-in
//!   set of CA roots** that ages. `polis-repo` currently has nine direct
//!   dependencies and `Cargo.lock` has not been touched by two rounds of work on
//!   this module's neighbour; a security review of this feature can currently be
//!   done by reading one file.
//! * The crate already made exactly this call once and wrote it down:
//!   `polis-repo/Cargo.toml` says `git log` is invoked as a subprocess "so there
//!   is no libgit2 dependency: the parse surface is one stable plumbing format,
//!   and it avoids a C build on every platform." An HTTPS POST is the same
//!   shape of problem with the same shape of answer.
//! * `polis-hook` must stay at zero dependencies (PRD §14, ADR-0036). Adding
//!   nothing to the workspace makes that guarantee impossible to erode by
//!   accident rather than merely tested.
//!
//! So there are two shipped transports and a trait over them:
//!
//! | Transport | Handles | How |
//! |---|---|---|
//! | [`PlainHttpTransport`] | `http://` | `std::net::TcpStream`, HTTP/1.1, written out here |
//! | [`CurlTransport`] | `http://`, `https://` | `curl` as a subprocess, using the OS trust store |
//! | [`DefaultTransport`] | both | plain for `http://`, curl for `https://` |
//!
//! A local Ollama therefore needs no external binary at all, and the tests
//! exercise the real client against a real socket rather than a mock. A hosted
//! endpoint needs `curl`, which ships in Windows 10 1803 and later
//! (`C:\Windows\System32\curl.exe`), in macOS, and in essentially every Linux
//! image. When it is absent the feature degrades exactly as it does with no key.
//!
//! **This is a trait so the decision is reversible.** Handing
//! [`crate::llm::run::LlmRunner::with_transport`] a `ureq`-backed
//! implementation is a new `impl Transport` in the caller's crate and no change
//! here.
//!
//! # The key never reaches an argument list
//!
//! `curl`'s command line is visible to every process on the machine. So a
//! [`HeaderValue::Secret`] is written to `curl --config -` **on stdin**, never
//! into `argv` and never into a file, and the request body — which carries no
//! key — goes to a temporary file that is deleted when the call returns.

use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use super::secret::{bound_message, scrub, Secret};

// ---------------------------------------------------------------------------
// The shape of a call
// ---------------------------------------------------------------------------

/// One header value, which either is or is not a credential.
///
/// The distinction is not cosmetic: it decides whether the value may appear in
/// a process argument list.
#[derive(Clone)]
pub enum HeaderValue {
    /// Ordinary. May go anywhere.
    Plain(String),
    /// A credential. Only ever written to a subprocess's stdin.
    ///
    /// `prefix` is the part that is *not* secret — `"Bearer "` for an
    /// `Authorization` header, empty for Anthropic's `x-api-key`. Keeping it
    /// separate is what lets the whole value be rendered in exactly one place.
    Secret {
        /// The non-secret scheme prefix, `""` when there is none.
        prefix: String,
        /// The credential.
        secret: Arc<Secret>,
    },
}

impl HeaderValue {
    /// A `Bearer` credential.
    pub fn bearer(secret: Arc<Secret>) -> Self {
        Self::Secret {
            prefix: "Bearer ".to_owned(),
            secret,
        }
    }

    /// A bare credential, with no scheme prefix.
    pub fn raw_secret(secret: Arc<Secret>) -> Self {
        Self::Secret {
            prefix: String::new(),
            secret,
        }
    }

    /// The value as it goes on the wire. **The only place a key is rendered.**
    fn render(&self) -> String {
        match self {
            Self::Plain(v) => v.clone(),
            Self::Secret { prefix, secret } => format!("{prefix}{}", secret.expose()),
        }
    }
}

impl fmt::Debug for HeaderValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain(v) => write!(f, "{v:?}"),
            Self::Secret { prefix, .. } => write!(f, "\"{prefix}<redacted>\""),
        }
    }
}

/// A `POST` with a JSON body.
///
/// `Debug`-safe by construction: the only field that can hold a key is a
/// [`HeaderValue::Secret`], which prints as `<redacted>`.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// Absolute URL, `http://` or `https://`.
    pub url: String,
    /// Headers in the order they are sent. `Content-Type` and `Content-Length`
    /// are added by the transport.
    pub headers: Vec<(String, HeaderValue)>,
    /// The request body, already serialised.
    pub body: String,
    /// Wall clock ceiling for the whole call.
    pub timeout: Duration,
}

impl HttpRequest {
    /// The key carried in this request, if any — for [`scrub`].
    pub fn secret(&self) -> Option<&Secret> {
        self.headers.iter().find_map(|(_, v)| match v {
            HeaderValue::Secret { secret, .. } => Some(&**secret),
            HeaderValue::Plain(_) => None,
        })
    }
}

/// What came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The body, as text. Bounded by the transport.
    pub body: String,
}

impl HttpResponse {
    /// True for 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The largest response body a transport will hold in memory.
///
/// A description request's answer is a few kilobytes. Anything approaching this
/// is an endpoint returning something other than what was asked for, and
/// reading it all is how a mis-pointed `base_url` turns into an allocation
/// failure.
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Why a call did not produce a response.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// The URL is not one this transport speaks.
    #[error("unsupported url: {0}")]
    UnsupportedUrl(String),
    /// No route, no listener, DNS failure.
    #[error("unreachable: {0}")]
    Unreachable(String),
    /// The call exceeded [`HttpRequest::timeout`].
    #[error("timed out")]
    Timeout,
    /// The transport itself is not usable — `curl` is not installed.
    #[error("transport unavailable: {0}")]
    Unavailable(String),
    /// Bytes arrived but were not a response.
    #[error("bad response: {0}")]
    BadResponse(String),
    /// Local I/O failed.
    #[error("io: {0}")]
    Io(String),
}

impl TransportError {
    /// True when trying again could plausibly work.
    ///
    /// An unreachable host and a timeout are worth one more attempt; an
    /// unsupported URL and a missing `curl` will be just as wrong in half a
    /// second.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Unreachable(_) | Self::Timeout | Self::Io(_))
    }
}

/// One `POST`, synchronously.
///
/// `Send + Sync` because [`crate::llm::run::LlmRunner`] shares one across a
/// bounded pool of threads.
pub trait Transport: fmt::Debug + Send + Sync {
    /// Sends `request` and returns the status and body.
    ///
    /// A non-2xx status is a [`HttpResponse`], **not** an error: the provider's
    /// own error body is the most useful thing there is when something is
    /// misconfigured, and throwing it away to raise a generic error is how
    /// "invalid model id" becomes "the LLM does not work".
    fn post(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError>;

    /// A short name for the report.
    fn name(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// URL parsing
// ---------------------------------------------------------------------------

/// The parts of an absolute HTTP URL this module needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlParts {
    /// `http` or `https`.
    pub scheme: String,
    /// Host name or IP literal, without brackets.
    pub host: String,
    /// Explicit or scheme default.
    pub port: u16,
    /// Path and query, beginning with `/`.
    pub target: String,
}

impl UrlParts {
    /// `Host:` header value — the port is omitted when it is the default.
    pub fn authority(&self) -> String {
        let default = if self.scheme == "https" { 443 } else { 80 };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Splits an absolute `http`/`https` URL.
///
/// Deliberately not a general URL parser: this only ever sees a `base_url` the
/// operator configured plus a fixed path, so userinfo, fragments and relative
/// forms are rejected rather than interpreted. A URL with userinfo in it is
/// refused outright — that is a credential in a configuration file, and the one
/// thing this feature must not normalise.
pub fn parse_url(url: &str) -> Result<UrlParts, TransportError> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| TransportError::UnsupportedUrl(url.to_owned()))?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(TransportError::UnsupportedUrl(url.to_owned()));
    }
    let (authority, target) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.contains('@') {
        return Err(TransportError::UnsupportedUrl(
            "a url with a password in it is not a configuration".to_owned(),
        ));
    }
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        // An IPv6 literal.
        let (host, tail) = stripped
            .split_once(']')
            .ok_or_else(|| TransportError::UnsupportedUrl(url.to_owned()))?;
        (host, tail.strip_prefix(':'))
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        (host, Some(port))
    } else {
        (authority, None)
    };
    if host.is_empty() {
        return Err(TransportError::UnsupportedUrl(url.to_owned()));
    }
    let port = match port {
        Some(p) => p
            .parse::<u16>()
            .map_err(|_| TransportError::UnsupportedUrl(url.to_owned()))?,
        None if scheme == "https" => 443,
        None => 80,
    };
    Ok(UrlParts {
        scheme,
        host: host.to_owned(),
        port,
        target: target.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// Plain HTTP, written out
// ---------------------------------------------------------------------------

/// HTTP/1.1 over `std::net::TcpStream`. `http://` only.
///
/// Enough of the protocol for a JSON `POST` to an endpoint we configured:
/// `Connection: close`, `Content-Length` on the way out, and both
/// `Content-Length` and `Transfer-Encoding: chunked` understood on the way back
/// — Ollama uses the latter.
///
/// This is the transport the tests drive, against a real listener on
/// `127.0.0.1`, so the end-to-end HTTP path in this crate is exercised by
/// `cargo test` with no network and no external binary.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlainHttpTransport;

impl Transport for PlainHttpTransport {
    fn name(&self) -> &'static str {
        "plain-http"
    }

    fn post(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        let url = parse_url(&request.url)?;
        if url.scheme != "http" {
            return Err(TransportError::UnsupportedUrl(format!(
                "{} needs TLS; this transport speaks http:// only",
                url.scheme
            )));
        }
        let deadline = std::time::Instant::now() + request.timeout;
        let mut stream = connect(&url, request.timeout)?;
        stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|e| TransportError::Io(e.to_string()))?;
        stream
            .set_write_timeout(Some(remaining(deadline)?))
            .map_err(|e| TransportError::Io(e.to_string()))?;

        let mut head = String::with_capacity(256);
        head.push_str("POST ");
        head.push_str(&url.target);
        head.push_str(" HTTP/1.1\r\nHost: ");
        head.push_str(&url.authority());
        head.push_str("\r\nUser-Agent: polis\r\nAccept: application/json\r\n");
        head.push_str("Connection: close\r\nContent-Type: application/json\r\n");
        for (name, value) in &request.headers {
            if name.eq_ignore_ascii_case("content-type")
                || name.eq_ignore_ascii_case("content-length")
                || name.eq_ignore_ascii_case("host")
                || name.eq_ignore_ascii_case("connection")
            {
                continue;
            }
            head.push_str(name);
            head.push_str(": ");
            head.push_str(&value.render());
            head.push_str("\r\n");
        }
        head.push_str("Content-Length: ");
        head.push_str(&request.body.len().to_string());
        head.push_str("\r\n\r\n");

        let write = stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(request.body.as_bytes()))
            .and_then(|()| stream.flush());
        write.map_err(io_error)?;

        let mut raw = Vec::with_capacity(8 * 1024);
        let read = (&mut stream)
            .take(MAX_RESPONSE_BYTES as u64)
            .read_to_end(&mut raw);
        // A server that closes without a clean shutdown is normal enough that
        // bytes already read win over the error.
        if let Err(error) = read {
            if raw.is_empty() {
                return Err(io_error(error));
            }
        }
        parse_response(&raw)
    }
}

/// How long is left before the deadline, or [`TransportError::Timeout`].
fn remaining(deadline: std::time::Instant) -> Result<Duration, TransportError> {
    let now = std::time::Instant::now();
    if now >= deadline {
        return Err(TransportError::Timeout);
    }
    Ok(deadline - now)
}

/// Classifies a `std::io::Error` from a socket.
#[allow(clippy::needless_pass_by_value)] // the error is consumed by `to_string`
fn io_error(error: std::io::Error) -> TransportError {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::TimedOut | ErrorKind::WouldBlock => TransportError::Timeout,
        ErrorKind::ConnectionRefused
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::NotConnected
        | ErrorKind::AddrNotAvailable
        | ErrorKind::HostUnreachable
        | ErrorKind::NetworkUnreachable => TransportError::Unreachable(error.to_string()),
        _ => TransportError::Io(error.to_string()),
    }
}

/// Resolves and connects, honouring the timeout on each candidate address.
fn connect(url: &UrlParts, timeout: Duration) -> Result<TcpStream, TransportError> {
    let addrs = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| TransportError::Unreachable(format!("{}: {e}", url.host)))?;
    let mut last = TransportError::Unreachable(format!("{} resolved to nothing", url.host));
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = io_error(error),
        }
    }
    Err(last)
}

/// Splits a raw HTTP/1.1 response into a status and a decoded body.
fn parse_response(raw: &[u8]) -> Result<HttpResponse, TransportError> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| TransportError::BadResponse("no header terminator".to_owned()))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.lines();
    let status_line = lines
        .next()
        .ok_or_else(|| TransportError::BadResponse("empty response".to_owned()))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            TransportError::BadResponse(format!(
                "no status in {:?}",
                bound_message(status_line, 80)
            ))
        })?;
    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().ok();
        }
    }
    let body_bytes = &raw[split + 4..];
    let body = if chunked {
        dechunk(body_bytes)?
    } else if let Some(len) = content_length {
        body_bytes[..len.min(body_bytes.len())].to_vec()
    } else {
        body_bytes.to_vec()
    };
    Ok(HttpResponse {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// Decodes `Transfer-Encoding: chunked`.
fn dechunk(mut input: &[u8]) -> Result<Vec<u8>, TransportError> {
    let mut out = Vec::with_capacity(input.len());
    loop {
        let Some(eol) = input.windows(2).position(|w| w == b"\r\n") else {
            // A truncated final chunk: keep what arrived rather than losing the
            // whole answer to a server that closed early.
            return Ok(out);
        };
        let header = String::from_utf8_lossy(&input[..eol]);
        let size_hex = header.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| TransportError::BadResponse(format!("bad chunk size {size_hex:?}")))?;
        input = &input[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        let take = size.min(input.len());
        out.extend_from_slice(&input[..take]);
        if take < size {
            return Ok(out);
        }
        input = &input[take..];
        if input.starts_with(b"\r\n") {
            input = &input[2..];
        }
    }
}

// ---------------------------------------------------------------------------
// curl
// ---------------------------------------------------------------------------

/// A sentinel `curl` writes after the body so the status is unambiguous.
///
/// `-w "%{http_code}"` alone would be ambiguous against a JSON body that ends in
/// a digit, which is rare and would fail exactly once, in production, on
/// somebody else's machine.
const STATUS_SENTINEL: &str = "\n<<<POLIS-HTTP-STATUS>>>";

/// `curl` as a subprocess. Speaks `https://` using the operating system's own
/// trust store.
///
/// See the module documentation for why this is a subprocess and not a crate.
/// The two things worth knowing at the call site:
///
/// * **The key goes in on stdin**, as a `--config -` line, so it never appears
///   in `argv` where any process on the machine can read it.
/// * **The body goes to a temporary file** in the platform state directory —
///   never in the repository (ADR-0065) — which is deleted before this returns.
///   The body carries no credential; putting it on stdin as well would collide
///   with the configuration, and escaping it into the configuration file would
///   make correctness depend on `curl`'s quoting rules.
#[derive(Debug, Clone)]
pub struct CurlTransport {
    program: std::ffi::OsString,
}

impl Default for CurlTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl CurlTransport {
    /// Uses `curl` from `PATH`, or `$POLIS_CURL` when set.
    pub fn new() -> Self {
        Self {
            program: std::env::var_os("POLIS_CURL").unwrap_or_else(|| "curl".into()),
        }
    }

    /// Uses a specific binary. The tests point this at a script to exercise the
    /// failure paths without a network.
    pub fn with_program(program: impl Into<std::ffi::OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// True when the binary can be executed at all.
    pub fn is_available(&self) -> bool {
        std::process::Command::new(&self.program)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

/// A temporary file that removes itself.
struct TempBody(std::path::PathBuf);

impl Drop for TempBody {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Serial for the temporary body file's name. Process id plus this is unique
/// within a run and between runs, and needs no wall clock — this crate reads no
/// clock (see the crate docs).
static TEMP_BODY_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl TempBody {
    /// Writes `body` somewhere outside the repository.
    ///
    /// The state directory when there is one, the platform temp directory
    /// otherwise. Never the checkout: ADR-0065.
    fn write(body: &str) -> Result<Self, TransportError> {
        let dir = crate::corpus::state_dir().map_or_else(std::env::temp_dir, |d| d.join("llm"));
        std::fs::create_dir_all(&dir).map_err(|e| TransportError::Io(e.to_string()))?;
        let n = TEMP_BODY_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!("req-{}-{n}.json", std::process::id()));
        std::fs::write(&path, body.as_bytes()).map_err(|e| TransportError::Io(e.to_string()))?;
        Ok(Self(path))
    }
}

/// Escapes a value for a `curl` configuration file's double-quoted string.
fn curl_config_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

impl Transport for CurlTransport {
    fn name(&self) -> &'static str {
        "curl"
    }

    // One subprocess invocation, argument by argument. Splitting the argv
    // construction from the spawn would hide which flags are set where, and the
    // flags are the security-relevant part.
    #[allow(clippy::too_many_lines)]
    fn post(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        // Rejects userinfo URLs and anything that is not http(s) before a
        // process is spawned.
        let _ = parse_url(&request.url)?;
        let body = TempBody::write(&request.body)?;
        let seconds = request.timeout.as_secs().max(1);

        let mut command = std::process::Command::new(&self.program);
        command
            .arg("--silent")
            .arg("--show-error")
            .arg("--config")
            .arg("-")
            .arg("--request")
            .arg("POST")
            .arg("--max-time")
            .arg(seconds.to_string())
            .arg("--connect-timeout")
            .arg(seconds.min(15).to_string())
            .arg("--proto")
            .arg("=http,https")
            .arg("--header")
            .arg("Content-Type: application/json")
            .arg("--header")
            .arg("Accept: application/json")
            .arg("--user-agent")
            .arg("polis")
            .arg("--data-binary")
            .arg({
                let mut at = std::ffi::OsString::from("@");
                at.push(&body.0);
                at
            })
            .arg("--write-out")
            .arg(format!("{STATUS_SENTINEL}%{{http_code}}"))
            .arg("--output")
            .arg("-");

        // Non-secret headers may go in `argv`; a secret may not.
        let mut config = String::new();
        for (name, value) in &request.headers {
            match value {
                HeaderValue::Plain(v) => {
                    command.arg("--header").arg(format!("{name}: {v}"));
                }
                HeaderValue::Secret { .. } => {
                    config.push_str("header = ");
                    config.push_str(&curl_config_quote(&format!("{name}: {}", value.render())));
                    config.push('\n');
                }
            }
        }
        command.arg("--url").arg(&request.url);

        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                TransportError::Unavailable(format!(
                    "{} is not on PATH; set POLIS_CURL or use an http:// endpoint",
                    self.program.to_string_lossy()
                ))
            } else {
                TransportError::Io(error.to_string())
            }
        })?;
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| TransportError::Io("no stdin".to_owned()))?;
            stdin
                .write_all(config.as_bytes())
                .map_err(|e| TransportError::Io(e.to_string()))?;
            // Dropped here, closing the pipe: `curl` reads the configuration to
            // end-of-file before it does anything else.
        }
        let output = child
            .wait_with_output()
            .map_err(|e| TransportError::Io(e.to_string()))?;
        drop(body);

        let secret = request.secret();
        if !output.status.success() {
            let stderr = scrub(&String::from_utf8_lossy(&output.stderr), secret);
            let message = bound_message(&stderr, 300);
            // `curl`'s documented exit codes. 28 is the only one worth
            // distinguishing, because it is the one a longer timeout fixes.
            return Err(match output.status.code() {
                Some(28) => TransportError::Timeout,
                Some(2 | 4 | 27 | 37 | 43) => TransportError::Unavailable(message),
                Some(code) => TransportError::Unreachable(format!("curl exit {code}: {message}")),
                None => TransportError::Unreachable(format!("curl was killed: {message}")),
            });
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let (body, status) = stdout
            .rsplit_once(STATUS_SENTINEL)
            .ok_or_else(|| TransportError::BadResponse("curl produced no status".to_owned()))?;
        let status: u16 = status
            .trim()
            .parse()
            .map_err(|_| TransportError::BadResponse(format!("curl status {status:?}")))?;
        if status == 0 {
            return Err(TransportError::Unreachable(bound_message(
                &scrub(&String::from_utf8_lossy(&output.stderr), secret),
                300,
            )));
        }
        Ok(HttpResponse {
            status,
            body: body.chars().take(MAX_RESPONSE_BYTES).collect(),
        })
    }
}

// ---------------------------------------------------------------------------
// The default
// ---------------------------------------------------------------------------

/// `http://` in process, `https://` through `curl`.
///
/// The shipped choice, and the reason a local Ollama needs nothing installed
/// while a hosted endpoint needs only what the operating system already ships.
#[derive(Debug, Clone, Default)]
pub struct DefaultTransport {
    plain: PlainHttpTransport,
    curl: CurlTransport,
}

impl DefaultTransport {
    /// The shipped transport.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this transport can reach `url` at all, and why not when it
    /// cannot. For the dry run's report, so "no TLS available" is visible before
    /// a bill rather than after a failure.
    pub fn readiness(&self, url: &str) -> Result<(), TransportError> {
        let parts = parse_url(url)?;
        if parts.scheme == "https" && !self.curl.is_available() {
            return Err(TransportError::Unavailable(
                "https needs curl, which was not found; set POLIS_CURL".to_owned(),
            ));
        }
        Ok(())
    }
}

impl Transport for DefaultTransport {
    fn name(&self) -> &'static str {
        "default"
    }

    fn post(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        let parts = parse_url(&request.url)?;
        if parts.scheme == "http" {
            self.plain.post(request)
        } else {
            self.curl.post(request)
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A real HTTP server, in process, for the tests.
    //!
    //! Not a mock of [`Transport`]: the point is to exercise
    //! [`PlainHttpTransport`] — socket, request framing, chunked decoding and
    //! all — against something that speaks HTTP, so the shipped path is the one
    //! under test.

    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    /// What the fake endpoint should do with the next request.
    #[derive(Debug, Clone)]
    pub(crate) enum Reply {
        /// A status and a body.
        Body(u16, String),
        /// The same, framed with `Transfer-Encoding: chunked`.
        Chunked(u16, String),
        /// Accept the connection and close it without answering.
        Hangup,
        /// Accept and never answer, so the client's timeout fires.
        Stall,
    }

    /// An in-process endpoint on `127.0.0.1`.
    #[derive(Debug)]
    pub(crate) struct FakeEndpoint {
        /// `http://127.0.0.1:<port>`.
        pub(crate) base: String,
        /// Every request body it received, in arrival order.
        pub(crate) seen: Arc<Mutex<Vec<String>>>,
        /// Every `Authorization` header it received.
        pub(crate) auth: Arc<Mutex<Vec<String>>>,
        /// How many requests it has answered.
        pub(crate) calls: Arc<AtomicU32>,
        listener: Arc<TcpListener>,
        stopping: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeEndpoint {
        /// Serves `replies` in order, repeating the last one for ever.
        pub(crate) fn start(replies: Vec<Reply>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let listener = Arc::new(listener);
            let seen = Arc::new(Mutex::new(Vec::new()));
            let auth = Arc::new(Mutex::new(Vec::new()));
            let calls = Arc::new(AtomicU32::new(0));
            let stopping = Arc::new(AtomicBool::new(false));
            let handle = {
                let listener = Arc::clone(&listener);
                let seen = Arc::clone(&seen);
                let auth = Arc::clone(&auth);
                let calls = Arc::clone(&calls);
                let stopping = Arc::clone(&stopping);
                std::thread::spawn(move || {
                    for stream in listener.incoming() {
                        let Ok(stream) = stream else { break };
                        if stopping.load(Ordering::SeqCst) {
                            break;
                        }
                        let n = calls.fetch_add(1, Ordering::SeqCst) as usize;
                        let reply = replies
                            .get(n)
                            .or_else(|| replies.last())
                            .cloned()
                            .unwrap_or(Reply::Hangup);
                        serve(stream, &reply, &seen, &auth, &stopping);
                    }
                })
            };
            Self {
                base: format!("http://127.0.0.1:{port}"),
                seen,
                auth,
                calls,
                listener,
                stopping,
                handle: Some(handle),
            }
        }

        /// How many requests it has answered — the retry counter.
        pub(crate) fn call_count(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }

        /// The bodies it received.
        pub(crate) fn bodies(&self) -> Vec<String> {
            self.seen.lock().expect("lock").clone()
        }

        /// The `Authorization` headers it received.
        pub(crate) fn authorizations(&self) -> Vec<String> {
            self.auth.lock().expect("lock").clone()
        }
    }

    impl Drop for FakeEndpoint {
        fn drop(&mut self) {
            // Set the flag first, then unblock `incoming()` with one connection.
            // Without the flag a `Stall` reply would serve the shutdown
            // connection too, and the join would wait out another sleep.
            self.stopping.store(true, Ordering::SeqCst);
            let addr = self.listener.local_addr().expect("addr");
            let _ = TcpStream::connect(addr);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn serve(
        mut stream: TcpStream,
        reply: &Reply,
        seen: &Arc<Mutex<Vec<String>>>,
        auth: &Arc<Mutex<Vec<String>>>,
        stopping: &Arc<AtomicBool>,
    ) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().unwrap_or(0);
                } else if name.eq_ignore_ascii_case("authorization") {
                    auth.lock().expect("lock").push(value.trim().to_owned());
                }
            }
        }
        let mut body = vec![0u8; length];
        let _ = reader.read_exact(&mut body);
        seen.lock()
            .expect("lock")
            .push(String::from_utf8_lossy(&body).into_owned());

        match reply {
            Reply::Hangup => {}
            Reply::Stall => {
                // Long enough for any client timeout under test, short enough
                // that a bug here costs seconds rather than a hung suite. The
                // flag lets `drop` cut it short.
                for _ in 0..50 {
                    if stopping.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            Reply::Body(status, text) => {
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    text.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(text.as_bytes());
            }
            Reply::Chunked(status, text) => {
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(head.as_bytes());
                // Two chunks, so the decoder is actually exercised.
                let (a, b) = text.split_at(text.len() / 2);
                for part in [a, b] {
                    let _ = stream.write_all(format!("{:x}\r\n", part.len()).as_bytes());
                    let _ = stream.write_all(part.as_bytes());
                    let _ = stream.write_all(b"\r\n");
                }
                let _ = stream.write_all(b"0\r\n\r\n");
            }
        }
        let _ = stream.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FakeEndpoint, Reply};
    use super::*;

    fn request(url: &str, body: &str) -> HttpRequest {
        HttpRequest {
            url: url.to_owned(),
            headers: Vec::new(),
            body: body.to_owned(),
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn urls_are_split_and_a_password_in_one_is_refused() {
        let parts = parse_url("https://api.z.ai/api/paas/v4/chat/completions").expect("url");
        assert_eq!(parts.scheme, "https");
        assert_eq!(parts.host, "api.z.ai");
        assert_eq!(parts.port, 443);
        assert_eq!(parts.target, "/api/paas/v4/chat/completions");
        assert_eq!(parts.authority(), "api.z.ai");

        let local = parse_url("http://localhost:11434/v1/chat/completions").expect("url");
        assert_eq!(local.port, 11434);
        assert_eq!(local.authority(), "localhost:11434");

        let bare = parse_url("http://example.test").expect("url");
        assert_eq!(bare.target, "/");

        for bad in [
            "ftp://example.test/x",
            "example.test/x",
            "https://user:pass@example.test/x",
            "http://:80/x",
        ] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_post_round_trips_against_a_real_socket() {
        let endpoint = FakeEndpoint::start(vec![Reply::Body(200, r#"{"ok":true}"#.to_owned())]);
        let transport = PlainHttpTransport;
        let response = transport
            .post(&request(
                &format!("{}/v1/chat", endpoint.base),
                r#"{"a":1}"#,
            ))
            .expect("a response");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, r#"{"ok":true}"#);
        assert!(response.is_success());
        assert_eq!(endpoint.bodies(), [r#"{"a":1}"#]);
    }

    #[test]
    fn a_chunked_body_is_decoded() {
        let payload = r#"{"choices":[{"message":{"content":"hello there"}}]}"#;
        let endpoint = FakeEndpoint::start(vec![Reply::Chunked(200, payload.to_owned())]);
        let response = PlainHttpTransport
            .post(&request(&format!("{}/v1/chat", endpoint.base), "{}"))
            .expect("a response");
        assert_eq!(response.body, payload);
    }

    #[test]
    fn a_header_reaches_the_server_and_a_secret_one_is_not_in_the_debug() {
        std::env::set_var("POLIS_TEST_TRANSPORT_KEY", "tok-abcdef123456");
        let key =
            Arc::new(Secret::from_env(&["POLIS_TEST_TRANSPORT_KEY".to_owned()]).expect("set"));
        std::env::remove_var("POLIS_TEST_TRANSPORT_KEY");
        let endpoint = FakeEndpoint::start(vec![Reply::Body(200, "{}".to_owned())]);
        let mut req = request(&format!("{}/v1/chat", endpoint.base), "{}");
        req.headers.push((
            "Authorization".to_owned(),
            HeaderValue::bearer(Arc::clone(&key)),
        ));
        let debug = format!("{req:?}");
        assert!(!debug.contains("tok-abcdef"), "{debug}");
        assert!(debug.contains("Bearer <redacted>"), "{debug}");
        PlainHttpTransport.post(&req).expect("a response");
        assert_eq!(endpoint.authorizations(), ["Bearer tok-abcdef123456"]);
        assert_eq!(
            req.secret().map(Secret::expose),
            Some("tok-abcdef123456"),
            "the runner needs it back to scrub error text"
        );
    }

    #[test]
    fn a_non_2xx_status_is_a_response_and_not_an_error() {
        // The provider's own error body is the most useful thing there is.
        let endpoint = FakeEndpoint::start(vec![Reply::Body(
            401,
            r#"{"error":{"message":"invalid api key"}}"#.to_owned(),
        )]);
        let response = PlainHttpTransport
            .post(&request(&format!("{}/v1/chat", endpoint.base), "{}"))
            .expect("a response, not an error");
        assert_eq!(response.status, 401);
        assert!(!response.is_success());
        assert!(response.body.contains("invalid api key"));
    }

    #[test]
    fn a_dead_port_is_unreachable_and_never_a_panic() {
        // Bind and drop, so the port is very unlikely to be listening.
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let error = PlainHttpTransport
            .post(&request(&format!("http://127.0.0.1:{dead}/v1/chat"), "{}"))
            .expect_err("a dead port");
        assert!(matches!(error, TransportError::Unreachable(_)), "{error:?}");
        assert!(error.is_retryable());
    }

    #[test]
    fn a_server_that_hangs_up_without_answering_is_an_error_not_a_panic() {
        let endpoint = FakeEndpoint::start(vec![Reply::Hangup]);
        let error = PlainHttpTransport
            .post(&request(&format!("{}/v1/chat", endpoint.base), "{}"))
            .expect_err("nothing came back");
        // Which of the two depends on the platform: a clean close gives no
        // header terminator (`BadResponse`), and Windows resets the connection
        // instead (`Unreachable`, WSAECONNRESET). Both are correct and both
        // degrade the same way; what matters is that neither is a panic and
        // neither is a successful parse of nothing.
        assert!(
            matches!(
                error,
                TransportError::BadResponse(_) | TransportError::Unreachable(_)
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_stalled_server_times_out_rather_than_hanging_the_caller() {
        let endpoint = FakeEndpoint::start(vec![Reply::Stall]);
        let mut req = request(&format!("{}/v1/chat", endpoint.base), "{}");
        req.timeout = Duration::from_millis(300);
        let error = PlainHttpTransport.post(&req).expect_err("a timeout");
        assert!(matches!(error, TransportError::Timeout), "{error:?}");
        assert!(error.is_retryable());
    }

    #[test]
    fn the_plain_transport_refuses_tls_rather_than_pretending() {
        let error = PlainHttpTransport
            .post(&request("https://api.z.ai/v1/chat", "{}"))
            .expect_err("no tls here");
        assert!(
            matches!(error, TransportError::UnsupportedUrl(_)),
            "{error:?}"
        );
        assert!(!error.is_retryable(), "retrying will not add TLS");
    }

    #[test]
    fn the_default_transport_sends_http_in_process() {
        let endpoint = FakeEndpoint::start(vec![Reply::Body(200, "{}".to_owned())]);
        let response = DefaultTransport::new()
            .post(&request(&format!("{}/v1/chat", endpoint.base), "{}"))
            .expect("a response");
        assert_eq!(response.status, 200);
        // And an http endpoint is ready whatever curl is doing.
        DefaultTransport::new()
            .readiness(&endpoint.base)
            .expect("http needs nothing installed");
    }

    #[test]
    fn a_missing_curl_is_unavailable_rather_than_a_crash() {
        let transport = CurlTransport::with_program("polis-no-such-program-xyz");
        assert!(!transport.is_available());
        let error = transport
            .post(&request("https://example.invalid/v1/chat", "{}"))
            .expect_err("no curl");
        assert!(matches!(error, TransportError::Unavailable(_)), "{error:?}");
        assert!(!error.is_retryable());
    }

    #[test]
    fn curl_speaks_to_the_fake_endpoint_when_it_is_installed() {
        let transport = CurlTransport::new();
        if !transport.is_available() {
            // Nothing to prove on a machine without curl; the degradation path
            // is covered by the test above.
            return;
        }
        let endpoint = FakeEndpoint::start(vec![Reply::Body(
            200,
            r#"{"choices":[{"message":{"content":"ok"}}]}"#.to_owned(),
        )]);
        std::env::set_var("POLIS_TEST_CURL_KEY", "curl-key-abcdef");
        let key = Arc::new(Secret::from_env(&["POLIS_TEST_CURL_KEY".to_owned()]).expect("set"));
        std::env::remove_var("POLIS_TEST_CURL_KEY");
        let mut req = request(&format!("{}/v1/chat", endpoint.base), r#"{"a":"b"}"#);
        req.headers
            .push(("Authorization".to_owned(), HeaderValue::bearer(key)));
        let response = transport.post(&req).expect("a response");
        assert_eq!(response.status, 200);
        assert!(response.body.contains("\"ok\""), "{response:?}");
        assert_eq!(endpoint.bodies(), [r#"{"a":"b"}"#]);
        // The key arrived, and it arrived through stdin rather than argv.
        assert_eq!(endpoint.authorizations(), ["Bearer curl-key-abcdef"]);
    }

    #[test]
    fn a_curl_config_value_is_escaped() {
        assert_eq!(curl_config_quote("plain"), "\"plain\"");
        assert_eq!(
            curl_config_quote(r#"a"b\c"#),
            r#""a\"b\\c""#,
            "quotes and backslashes"
        );
        assert_eq!(curl_config_quote("a\nb"), "\"a\\nb\"");
    }

    #[test]
    fn a_response_with_no_headers_is_rejected_rather_than_guessed() {
        assert!(parse_response(b"garbage").is_err());
        assert!(parse_response(b"HTTP/1.1 nope\r\n\r\n").is_err());
        let ok = parse_response(b"HTTP/1.1 204 No Content\r\n\r\n").expect("parsed");
        assert_eq!(ok.status, 204);
        assert!(ok.body.is_empty());
    }

    #[test]
    fn a_truncated_chunked_body_keeps_what_arrived() {
        // A server that closed mid-chunk should not lose the whole answer.
        let partial = b"5\r\nhello\r\n5\r\nwor";
        assert_eq!(dechunk(partial).expect("decoded"), b"hellowor");
        assert!(dechunk(b"zz\r\n").is_err(), "a bad chunk size is an error");
    }
}
