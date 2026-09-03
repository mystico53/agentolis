//! `polis-sessiond` — the process that owns the agents (PRD §15 M7, ADR-0098).
//!
//! Polis's window is a view. This is the thing being viewed: it holds the ptys,
//! so closing the window, or a GPU driver reset — not rare on Windows — costs a
//! view and not an afternoon's work. tmux, herdr and cmux each arrived at the
//! same split independently; `docs/roadmap/terminal-integration.md` planned it as
//! M8 and it is built first, because doing it later means doing it twice, and
//! because the pty's readiness model wants an event loop the window has nowhere
//! to put (see `polis_term::pty`).
//!
//! # What it is not
//!
//! It is **not a server** in PRD §2's sense. It binds `127.0.0.1` only, it never
//! speaks to the network, nothing leaves the box, and there is no account and no
//! cloud. PRD §2's non-goal says "no server" and means those three things; this
//! is a local background process, and §2 needs the clause spelled out rather
//! than quietly reinterpreted.
//!
//! It is also deliberately **dumb**. It never parses a byte of what a pty
//! produces; it appends to a log and forwards. Every VT escape, every colour and
//! every cursor movement is the window's problem, which is what lets the parser
//! and the widget live entirely on that side (`polis_term::proto`).
//!
//! # The orphan problem, answered
//!
//! The failure that makes people distrust background processes is one left
//! running with agents nobody is watching. Three things answer it:
//!
//! * `--idle-timeout` (default 10 minutes): with **no panes and no clients**,
//!   the daemon exits on its own. It never exits while an agent is running.
//! * `polis-sessiond --status` says what is running, from any terminal.
//! * `polis-sessiond --stop` ends it, and the endpoint file names its pid so
//!   even a wedged one can be found.

use std::io::IsTerminal as _;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use polis_term::proto::{Call, Message, Reply, Request, PROTOCOL};
use polis_term::transport::{self, Endpoint, Frames, Wire};

mod panes;

use panes::{ClientId, Panes};

/// How often the idle watchdog looks.
const WATCHDOG_TICK: Duration = Duration::from_secs(15);

/// How long one message may take to reach a client before that client is
/// treated as gone. Bounds the writer join in [`converse`].
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// The session daemon: it owns the ptys, so the agents outlive the window.
#[derive(Debug, Parser)]
#[command(name = "polis-sessiond", version, about)]
struct Cli {
    /// Where the endpoint file and the pane logs live.
    ///
    /// Defaults to the same state directory the window uses, which is how they
    /// find each other.
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// The port to prefer. A busy one moves rather than failing.
    #[arg(long, default_value_t = transport::DEFAULT_PORT)]
    port: u16,

    /// Exit after this many seconds with no panes **and** no clients.
    ///
    /// Zero disables it. A daemon with a live agent never idles out, whatever
    /// this says.
    #[arg(long, default_value_t = 600)]
    idle_timeout: u64,

    /// Print what a running daemon is doing, and exit.
    #[arg(long, conflicts_with = "stop")]
    status: bool,

    /// Tell a running daemon to close every pane and exit.
    #[arg(long)]
    stop: bool,
}

fn main() -> anyhow::Result<()> {
    // First, before any thread exists: a daemon launched from inside a Claude
    // Code session inherits that session's identity markers, and one of them
    // turns transcript saving off for every agent it goes on to start. See
    // `polis_term::pty::INHERITED_AGENT_MARKERS` — this was measured, not
    // guessed.
    let cleared = polis_term::pty::disinherit_agent_markers();

    let cli = Cli::parse();
    let Some(state_dir) = cli.state_dir.clone().or_else(transport::default_state_dir) else {
        anyhow::bail!(
            "no state directory: set --state-dir, or LOCALAPPDATA (Windows) / \
             XDG_STATE_HOME (elsewhere)"
        )
    };

    if cli.status {
        return status(&state_dir);
    }
    if cli.stop {
        stop(&state_dir);
        return Ok(());
    }
    serve(&cli, &state_dir, &cleared)
}

/// Prints what a running daemon is doing.
fn status(state_dir: &std::path::Path) -> anyhow::Result<()> {
    let Some(endpoint) = transport::discover(state_dir) else {
        println!(
            "no session daemon (nothing published in {})",
            state_dir.display()
        );
        return Ok(());
    };
    let client =
        match polis_term::SessionClient::connect(&endpoint, "polis-sessiond --status", || {}) {
            Ok(client) => client,
            Err(error) => {
                println!(
                    "a stale endpoint names pid {} at {}, and it is not answering: {error}",
                    endpoint.pid, endpoint.addr
                );
                return Ok(());
            }
        };
    println!(
        "session daemon {} (pid {}) on {}, protocol {}",
        client.daemon_version, endpoint.pid, endpoint.addr, endpoint.protocol
    );
    match client.call(Request::List).map_err(anyhow::Error::msg)? {
        Reply::Panes { panes } if panes.is_empty() => println!("  no panes"),
        Reply::Panes { panes } => {
            for pane in panes {
                let state = match pane.exit {
                    None => format!("running as pid {}", pane.pid.unwrap_or(0)),
                    Some(code) => format!("exited {code}"),
                };
                println!(
                    "  {} {} — {} — {} — {} bytes of history",
                    pane.pane,
                    pane.program,
                    state,
                    pane.session_id
                        .as_ref()
                        .map_or_else(|| "no session id".to_owned(), ToString::to_string),
                    pane.bytes
                );
            }
        }
        other => println!("  the daemon answered list with {other:?}"),
    }
    Ok(())
}

/// Tells a running daemon to stop.
fn stop(state_dir: &std::path::Path) {
    let Some(endpoint) = transport::discover(state_dir) else {
        println!("no session daemon is running");
        return;
    };
    match polis_term::SessionClient::connect(&endpoint, "polis-sessiond --stop", || {}) {
        Ok(client) => {
            let _ = client.call(Request::Shutdown);
            println!("asked the session daemon (pid {}) to stop", endpoint.pid);
        }
        Err(error) => {
            println!("no session daemon answered at {}: {error}", endpoint.addr);
            println!("removing the stale endpoint file");
            transport::withdraw(state_dir);
        }
    }
}

/// Binds, publishes, and serves until told to stop.
fn serve(cli: &Cli, state_dir: &std::path::Path, cleared: &[&str]) -> anyhow::Result<()> {
    if let Some(existing) = transport::discover(state_dir) {
        if polis_term::SessionClient::connect(&existing, "polis-sessiond --probe", || {}).is_ok() {
            anyhow::bail!(
                "a session daemon is already running as pid {} on {} — \
                 use `polis-sessiond --status`, or `--stop` first",
                existing.pid,
                existing.addr
            );
        }
        // Only a stale file. Taking it over is the right move; leaving it makes
        // every future client report a daemon that is not there.
        transport::withdraw(state_dir);
    }

    let listener = transport::bind(cli.port)?;
    let addr = listener.local_addr()?;
    let endpoint = Endpoint {
        addr,
        token: transport::fresh_token(),
        pid: std::process::id(),
        protocol: PROTOCOL,
    };
    transport::publish(state_dir, &endpoint)?;

    let panes = Panes::start(state_dir.join("panes")).map_err(anyhow::Error::msg)?;
    let stopping = Arc::new(AtomicBool::new(false));

    if std::io::stderr().is_terminal() {
        eprintln!(
            "polis session daemon on {addr} (pid {}), state in {}",
            endpoint.pid,
            state_dir.display()
        );
        if !cleared.is_empty() {
            eprintln!(
                "  cleared {} inherited agent marker(s) so panes get their own \
                 session: {}",
                cleared.len(),
                cleared.join(", ")
            );
        }
        eprintln!("  polis-sessiond --status   what is running");
        eprintln!("  polis-sessiond --stop     close every pane and exit");
    }

    watchdog(&panes, &stopping, addr, cli.idle_timeout);
    tracing::info!("cleared inherited agent markers: {cleared:?}");

    for incoming in listener.incoming() {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        let Ok(stream) = incoming else { continue };
        let panes = Arc::clone(&panes);
        let stopping = Arc::clone(&stopping);
        let token = endpoint.token.clone();
        let _ = std::thread::Builder::new()
            .name("polis-sessiond-conn".to_owned())
            .spawn(move || {
                if let Err(error) = converse(&panes, &stopping, &token, &stream) {
                    tracing::debug!("connection ended: {error}");
                }
            });
    }

    panes.close_all("the session daemon is shutting down");
    transport::withdraw(state_dir);
    Ok(())
}

/// Exits a daemon that has nothing to look after.
///
/// The check is deliberately conservative: **no panes and no clients**. A pane
/// that has exited still counts, because its last screen and its exit code are
/// the reason somebody will look.
fn watchdog(
    panes: &Arc<Panes>,
    stopping: &Arc<AtomicBool>,
    addr: std::net::SocketAddr,
    seconds: u64,
) {
    if seconds == 0 {
        return;
    }
    let panes = Arc::clone(panes);
    let stopping = Arc::clone(stopping);
    let limit = Duration::from_secs(seconds);
    let _ = std::thread::Builder::new()
        .name("polis-sessiond-idle".to_owned())
        .spawn(move || {
            let mut empty_since: Option<Instant> = None;
            loop {
                std::thread::sleep(WATCHDOG_TICK);
                if stopping.load(Ordering::Acquire) {
                    return;
                }
                if panes.count() == 0 && panes.client_count() == 0 {
                    let since = *empty_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= limit {
                        tracing::info!("idle for {seconds}s with nothing to hold; exiting");
                        begin_shutdown(&stopping, addr);
                        return;
                    }
                } else {
                    empty_since = None;
                }
            }
        });
}

/// Sets the flag and wakes the accept loop.
///
/// `TcpListener::incoming` has no timeout and no cancel. Connecting to our own
/// address is the standard, dependency-free way to make it return once more so
/// it can notice the flag.
fn begin_shutdown(stopping: &Arc<AtomicBool>, addr: std::net::SocketAddr) {
    stopping.store(true, Ordering::Release);
    let _ = TcpStream::connect(addr);
}

/// One connection, from `hello` to end of stream.
fn converse(
    panes: &Arc<Panes>,
    stopping: &Arc<AtomicBool>,
    token: &str,
    stream: &TcpStream,
) -> anyhow::Result<()> {
    stream.set_nodelay(true)?;
    let (client, inbox) = panes.connect();
    let outbox = panes
        .outbox(client)
        .ok_or_else(|| anyhow::anyhow!("the registry forgot a connection it just made"))?;

    let writing = stream.try_clone()?;
    // Bounds the join below. A client that has stopped reading but not closed
    // its socket would otherwise hold this thread for as long as it liked.
    writing.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let writer = std::thread::Builder::new()
        .name("polis-sessiond-write".to_owned())
        .spawn(move || {
            let mut wire = Wire::new(writing);
            for message in inbox {
                if wire.send(&message).is_err() {
                    break;
                }
            }
        })?;

    let mut frames = Frames::new(stream.try_clone()?);
    let mut greeted = false;
    let addr = stream.local_addr().ok();
    while let Some(call) = frames.recv::<Call>()? {
        let Call { id, request } = call;
        let outcome = if greeted {
            act(panes, client, request)
        } else {
            match greet(token, request) {
                Ok(reply) => {
                    greeted = true;
                    Ok(reply)
                }
                Err(error) => {
                    let _ = outbox.send(Message::Failed { id, error });
                    break;
                }
            }
        };
        let message = match outcome {
            Ok(Outcome::Reply(result)) => Message::Reply { id, result },
            Ok(Outcome::Shutdown) => {
                let _ = outbox.send(Message::Reply {
                    id,
                    result: Reply::Done,
                });
                if let Some(addr) = addr {
                    begin_shutdown(stopping, addr);
                }
                break;
            }
            Err(error) => Message::Failed { id, error },
        };
        if outbox.send(message).is_err() {
            break;
        }
    }

    panes.disconnect(client);
    // Order matters, and getting it wrong is invisible in every test that does
    // not check a *refusal*: dropping the last sender ends the writer's loop,
    // and only once it has drained may the socket be closed. Shutting the
    // stream down first threw away the `Failed` that says why — so a client
    // presenting the wrong token was told "connection closed", which is exactly
    // the unhelpful message the protocol exists to avoid.
    drop(outbox);
    let _ = writer.join();
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Ok(())
}

/// What serving one call produced.
enum Outcome {
    Reply(Reply),
    Shutdown,
}

/// The first call on every connection, and the only one that is not authorised.
///
/// Version skew is refused **by name**, in both directions, because a daemon
/// left running across an upgrade is the expected case: an operator who reads
/// "protocol 2 versus 1" knows what to do, and one who reads "connection reset"
/// does not.
fn greet(token: &str, request: Request) -> Result<Outcome, String> {
    let Request::Hello {
        protocol,
        client,
        token: offered,
    } = request
    else {
        return Err("the first call on a connection must be hello".to_owned());
    };
    if protocol != PROTOCOL {
        return Err(format!(
            "this session daemon speaks protocol {PROTOCOL} and the client speaks {protocol} — \
             stop the daemon with `polis-sessiond --stop` and let the new one start it"
        ));
    }
    if !transport::token_matches(token, &offered) {
        return Err(
            "wrong token — read it from the endpoint file in the state directory".to_owned(),
        );
    }
    tracing::info!("{client} connected");
    Ok(Outcome::Reply(Reply::Hello {
        protocol: PROTOCOL,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
    }))
}

/// Every call after `hello`.
fn act(panes: &Arc<Panes>, client: ClientId, request: Request) -> Result<Outcome, String> {
    let reply = match request {
        Request::Hello { .. } => return Err("hello was already said".to_owned()),
        Request::List => Reply::Panes {
            panes: panes.list(),
        },
        Request::Open(open) => {
            let info = panes.open(&open)?;
            // Opening a pane attaches the client that asked for it: there is no
            // case where a window opens an agent and then does not want to see
            // it, and a separate round trip is a screen's worth of output that
            // could arrive in between.
            panes.attach(client, info.pane, 0)?;
            Reply::Opened {
                info: Box::new(info),
            }
        }
        Request::Write { pane, data } => {
            panes.write(pane, data)?;
            Reply::Done
        }
        Request::Resize { pane, rows, cols } => {
            panes.resize(pane, rows, cols)?;
            Reply::Done
        }
        Request::Attach { pane, since } => {
            panes.attach(client, pane, since)?;
            Reply::Done
        }
        Request::Detach { pane } => {
            panes.detach(client, pane);
            Reply::Done
        }
        Request::Close { pane } => {
            panes.close(pane)?;
            Reply::Done
        }
        Request::Shutdown => return Ok(Outcome::Shutdown),
    };
    Ok(Outcome::Reply(reply))
}
