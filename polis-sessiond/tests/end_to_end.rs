//! The whole stack, in one process pair: a real daemon binary, a real socket, a
//! real pty, and a real VT parser producing a screen.
//!
//! These are the tests that would have caught every mistake the unit tests
//! cannot see — the token check, the framing, the order of `Opened` against the
//! first `Output`, and the reattach that is the reason the daemon exists.

use std::io::Write as _;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use polis_term::proto::{OpenPane, PaneId, PaneInfo, Reply, Request};
use polis_term::transport::{self, Endpoint};
use polis_term::SessionClient;

/// A daemon started for one test, killed when the test ends.
struct Daemon {
    child: Child,
    endpoint: Endpoint,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    /// Starts the real binary on an ephemeral port in a temporary state
    /// directory, and waits for it to publish.
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let child = Command::new(env!("CARGO_BIN_EXE_polis-sessiond"))
            .arg("--state-dir")
            .arg(dir.path())
            .args(["--port", "0", "--idle-timeout", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the daemon binary should start");

        let started = Instant::now();
        let endpoint = loop {
            if let Some(endpoint) = transport::discover(dir.path()) {
                break endpoint;
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "the daemon never published an endpoint file"
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        Self {
            child,
            endpoint,
            dir,
        }
    }

    fn connect(&self, name: &str) -> SessionClient {
        SessionClient::connect(&self.endpoint, name, || {}).expect("connect")
    }
}

/// Runs a client's inbox into an emulator until `want` shows up on the screen.
///
/// Returns the screen as text either way, so a failure shows what did arrive.
fn screen_until(client: &SessionClient, pane: PaneId, want: &str, deadline: Duration) -> String {
    let started = Instant::now();
    let mut last = String::new();
    while started.elapsed() < deadline {
        // The client's reader thread has already parsed everything that has
        // arrived; the test only has to look.
        if let Some(state) = client.pane(pane) {
            let emulator = state.emulator.lock().expect("emulator");
            last = emulator.snapshot().text();
            drop(emulator);
            if last.contains(want) {
                return last;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    last
}

fn open(program: &str, args: &[&str]) -> OpenPane {
    OpenPane {
        program: program.to_owned(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        cwd: std::env::current_dir().expect("cwd"),
        env: vec![("POLIS_PANE".to_owned(), "end-to-end".to_owned())],
        rows: 24,
        cols: 80,
        session_id: None,
    }
}

fn echoing(text: &str) -> OpenPane {
    if cfg!(windows) {
        open("cmd", &["/c", &format!("echo {text}")])
    } else {
        open("sh", &["-c", &format!("echo {text}")])
    }
}

fn opened(reply: Reply) -> PaneInfo {
    match reply {
        Reply::Opened { info } => *info,
        other => panic!("expected an opened pane, got {other:?}"),
    }
}

/// The headline: a program runs in the daemon and a screen appears in a client.
#[test]
fn a_program_runs_in_the_daemon_and_draws_a_screen_in_the_client() {
    let daemon = Daemon::start();
    let client = daemon.connect("end-to-end");
    assert!(client.connected());
    assert!(!client.daemon_version.is_empty(), "hello carried a version");

    let info = opened(
        client
            .call(Request::Open(Box::new(echoing("through-the-whole-stack"))))
            .expect("open"),
    );
    let screen = screen_until(
        &client,
        info.pane,
        "through-the-whole-stack",
        Duration::from_secs(30),
    );
    assert!(
        screen.contains("through-the-whole-stack"),
        "the screen was: {screen:?}"
    );
}

/// The reason the daemon exists. The first window opens an agent and goes away;
/// a second window, which never saw any of it, gets the same screen.
#[test]
fn a_second_window_reattaches_to_a_pane_the_first_one_opened() {
    let daemon = Daemon::start();

    let pane = {
        let first = daemon.connect("first-window");
        let info = opened(
            first
                .call(Request::Open(Box::new(echoing("survives-the-window"))))
                .expect("open"),
        );
        let screen = screen_until(
            &first,
            info.pane,
            "survives-the-window",
            Duration::from_secs(30),
        );
        assert!(screen.contains("survives-the-window"), "{screen:?}");
        info.pane
        // `first` drops here: the window is gone.
    };

    let second = daemon.connect("second-window");
    let Reply::Panes { panes } = second.call(Request::List).expect("list") else {
        panic!("expected a pane list");
    };
    let info = panes
        .into_iter()
        .find(|p| p.pane == pane)
        .expect("the pane outlived the window that opened it");
    second.adopt(info).expect("adopt");

    let screen = screen_until(
        &second,
        pane,
        "survives-the-window",
        Duration::from_secs(20),
    );
    assert!(
        screen.contains("survives-the-window"),
        "reattach replays the byte log through a fresh parser: {screen:?}"
    );
}

/// A keystroke has to cross two processes and a pty and come back as a screen.
#[test]
fn a_keystroke_crosses_the_daemon_and_comes_back_as_a_screen() {
    let daemon = Daemon::start();
    let client = daemon.connect("typing");
    let request = if cfg!(windows) {
        open("cmd", &["/q", "/k"])
    } else {
        open("sh", &[])
    };
    let info = opened(client.call(Request::Open(Box::new(request))).expect("open"));

    client
        .send(Request::Write {
            pane: info.pane,
            data: b"echo round-trip-worked\r".to_vec(),
        })
        .expect("write");

    let screen = screen_until(
        &client,
        info.pane,
        "round-trip-worked",
        Duration::from_secs(30),
    );
    assert!(screen.contains("round-trip-worked"), "{screen:?}");
    client
        .call(Request::Close { pane: info.pane })
        .expect("close");
}

/// The token is the only thing between a loopback port that spawns processes
/// and every other program on the machine. It has to actually be checked.
#[test]
fn a_client_without_the_token_is_refused() {
    let daemon = Daemon::start();
    let mut wrong = daemon.endpoint.clone();
    wrong.token = "0".repeat(64);
    let error = SessionClient::connect(&wrong, "impostor", || {}).expect_err("must be refused");
    assert!(error.to_lowercase().contains("token"), "{error}");

    // And the daemon is still perfectly happy to serve a client that has it.
    let good = daemon.connect("legitimate");
    assert!(good.connected());
}

/// A daemon left running across an upgrade is the expected case, not the
/// exotic one, so the mismatch has to be a sentence and not a reset connection.
#[test]
fn a_protocol_mismatch_is_refused_by_name() {
    let daemon = Daemon::start();
    let mut future = daemon.endpoint.clone();
    future.protocol = polis_term::proto::PROTOCOL + 1;
    let error = SessionClient::connect(&future, "from-the-future", || {}).expect_err("refused");
    assert!(error.contains("protocol"), "{error}");
    assert!(
        error.contains(&polis_term::proto::PROTOCOL.to_string()),
        "the message must name both versions: {error}"
    );
}

/// `--status` is how an operator finds out what a background process is doing,
/// so it has to work from a cold start with no window involved.
#[test]
fn the_status_command_reports_a_running_pane() {
    let daemon = Daemon::start();
    let client = daemon.connect("status-test");
    let info = opened(
        client
            .call(Request::Open(Box::new(echoing("visible-in-status"))))
            .expect("open"),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_polis-sessiond"))
        .arg("--state-dir")
        .arg(daemon.dir.path())
        .arg("--status")
        .output()
        .expect("status");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("session daemon"), "{text}");
    assert!(text.contains(&info.pane.to_string()), "{text}");
    assert!(text.contains(&info.program), "{text}");
}

/// A daemon that is not there must not look like one that is.
#[test]
fn a_stale_endpoint_file_does_not_look_like_a_daemon() {
    let dir = tempfile::tempdir().expect("tempdir");
    let endpoint = Endpoint {
        // Port 1 on loopback: nothing is listening, and nothing may.
        addr: "127.0.0.1:1".parse().expect("addr"),
        token: transport::fresh_token(),
        pid: 999_999,
        protocol: polis_term::proto::PROTOCOL,
    };
    transport::publish(dir.path(), &endpoint).expect("publish");
    assert!(SessionClient::connect(&endpoint, "hopeful", || {}).is_err());

    // And `--stop` cleans it up rather than leaving a lie in place.
    let output = Command::new(env!("CARGO_BIN_EXE_polis-sessiond"))
        .arg("--state-dir")
        .arg(dir.path())
        .arg("--stop")
        .output()
        .expect("stop");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("stale"), "{text}");
    assert!(
        transport::discover(dir.path()).is_none(),
        "the stale file should be gone"
    );
}

// ---------------------------------------------------------------------------
// Claude Code itself
// ---------------------------------------------------------------------------

/// The real thing, under a `ConPTY` host that is not Windows Terminal.
///
/// Ignored by default because it needs Claude Code installed; run it with
/// `cargo test -p polis-sessiond -- --ignored`. It is the test that answers the
/// three questions the roadmap listed as genuinely unknown: whether the TUI uses
/// the alternate screen, whether it emits synchronized-output sequences that
/// must at least be ignored cleanly, and — the dangerous one — whether it blocks
/// on a `DA1`/`DSR` reply at start-up.
#[test]
#[ignore = "needs Claude Code installed"]
fn claude_code_draws_its_interface_in_a_pane() {
    if polis_term::exec::which("claude").is_none() {
        eprintln!("no `claude` on PATH; nothing to test");
        return;
    }
    let daemon = Daemon::start();
    let client = daemon.connect("claude-test");

    let mut request = open("claude", &[]);
    request.rows = 45;
    request.cols = 120;
    request
        .env
        .push(("TERM".to_owned(), "xterm-256color".to_owned()));
    request
        .env
        .push(("COLORTERM".to_owned(), "truecolor".to_owned()));
    let session = polis_term::proto::fresh_session_id(0);
    request.args = vec!["--session-id".to_owned(), session.as_str().to_owned()];
    request.session_id = Some(session);

    let info = opened(
        client
            .call(Request::Open(Box::new(request)))
            .expect("open claude"),
    );

    // Anything at all on screen means the start-up handshake completed. A
    // terminal that does not answer DSR/DA1 leaves Claude Code drawing nothing,
    // forever, which is exactly the failure this asserts against.
    let started = Instant::now();
    let mut screen = String::new();
    while started.elapsed() < Duration::from_secs(60) {
        if let Some(state) = client.pane(info.pane) {
            let emulator = state.emulator.lock().expect("emulator");
            screen = emulator.snapshot().text();
            drop(emulator);
            if screen.chars().filter(|c| !c.is_whitespace()).count() > 40 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let path = std::env::temp_dir().join("polis-claude-pane-screen.txt");
    if let Ok(mut log) = std::fs::File::create(&path) {
        let _ = writeln!(log, "{screen}");
        eprintln!("pane screen written to {}", path.display());
    }

    assert!(
        screen.chars().filter(|c| !c.is_whitespace()).count() > 40,
        "Claude Code drew nothing in 60 s — the start-up query was probably not \
         answered. Screen was: {screen:?}"
    );
    assert!(
        !screen.contains('\u{fffd}'),
        "a replacement character means the parser dropped a byte: {screen:?}"
    );
    client
        .call(Request::Close { pane: info.pane })
        .expect("close");
}

/// The daemon must not leave an agent behind when it is told to stop.
#[test]
fn shutting_the_daemon_down_takes_its_panes_with_it() {
    let daemon = Daemon::start();
    let client = daemon.connect("shutdown-test");
    let long_running = if cfg!(windows) {
        open("cmd", &["/c", "pause"])
    } else {
        open("sh", &["-c", "sleep 300"])
    };
    let info = opened(
        client
            .call(Request::Open(Box::new(long_running)))
            .expect("open"),
    );
    let pid = info.pid.expect("the platform should name the child");

    let _ = client.call(Request::Shutdown);
    drop(client);

    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(20) {
        if !process_exists(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("pid {pid} outlived the daemon that started it");
}

/// Whether a process id is still live, without an `unsafe` block or a crate.
fn process_exists(pid: u32) -> bool {
    let output = if cfg!(windows) {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
    } else {
        Command::new("kill").args(["-0", &pid.to_string()]).output()
    };
    match output {
        Ok(out) if cfg!(windows) => String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()),
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}
