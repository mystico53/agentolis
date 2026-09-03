//! One child process on one pseudoterminal (ADR-0095).
//!
//! This is the daemon's half and the only part of Polis that owns an agent's
//! process. It parses nothing: bytes come off the pty and go straight to a sink.
//!
//! # The measurement that decided the architecture
//!
//! `docs/roadmap/terminal-integration.md` planned a reader thread shaped like a
//! socket:
//!
//! ```text
//! loop {
//!     match reader.read(&mut buf) {
//!         Ok(0) | Err(_) => break,   // EOF == the child's console closed
//!         Ok(n) => emulator.feed(&buf[..n]),
//!     }
//! }
//! ```
//!
//! **That loop exits immediately on Windows**, having read nothing. Measured:
//! `Ok(0)` after **6.8 µs**, zero bytes. `alacritty_terminal`'s Windows master is
//! an `UnblockedReader`, whose `Read` impl is a *non-blocking* drain of an
//! internal `piper` pipe — `Ok(0)` means "nothing right now", not end of file,
//! and end of file is reported separately through `EventedPty::next_child_event`.
//! Readiness comes from a `polling::Poller`, and from nowhere else.
//!
//! The same spike run against a `Poller` read 89 bytes of real `ConPTY` output in
//! two reads and saw a clean `Exited(ExitStatus(0))` at 13.4 ms.
//!
//! This is *why the session daemon exists rather than a pty in the window*. A
//! process whose main thread belongs to winit has nowhere natural to put a
//! poller loop; a daemon is one. The inversion of M7 and M8 made the hard part
//! easier rather than harder — see the crate docs.
//!
//! # `Ok(0)` means two different things
//!
//! On Unix the master is a non-blocking fd and `Ok(0)` **is** end of file; on
//! Windows it is "no data". [`ZERO_READ_IS_EOF`] is that difference, named, so
//! the read loop cannot quietly acquire one platform's assumption.

use std::collections::VecDeque;
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{
    self, ChildEvent, EventedPty, EventedReadWrite, Options, Pty, Shell,
};
use crossbeam_channel::{Receiver, Sender};
use polling::{Event, PollMode, Poller};

use crate::exec;

/// Whether a `read` of zero bytes from the pty master means end of file.
///
/// False on Windows, where the master is a non-blocking drain and exit is
/// reported by the child watcher instead. See the module docs — this constant
/// exists because getting it backwards costs either a pane that closes itself
/// 7 µs after opening, or one that never notices its agent died.
pub const ZERO_READ_IS_EOF: bool = !cfg!(windows);

/// How much pty output is taken per read.
///
/// 64 KiB matches the roadmap's figure and is comfortably above a single Ink
/// repaint of a full screen (~20 KiB with colour), so a busy frame is one read.
const READ_CHUNK: usize = 64 * 1024;

/// The poller's backstop timeout.
///
/// The loop is woken by the pty becoming readable, by the child exiting and by
/// [`Poller::notify`] when a command is queued, so this fires only when nothing
/// at all is happening: two wake-ups per second per pane, each a few
/// microseconds. It exists so that a missed notification costs half a second of
/// latency rather than a pane that has stopped forever.
const IDLE_WAIT: Duration = Duration::from_millis(500);

/// The retry interval while a write has not been fully accepted.
const BUSY_WAIT: Duration = Duration::from_millis(4);

/// How long a closed pty is given to end its child before force is used.
///
/// Closing the pseudoconsole is the graceful path: a well-behaved Node app sees
/// its console disappear and exits, and Claude Code gets to flush its
/// transcript. `ConPTY`'s own `Drop` blocks until the output pipe is drained, so
/// most of this budget is normally already spent by the time it is measured.
const GRACE: Duration = Duration::from_millis(500);

/// The poller key. One source, one key.
const READ_KEY: usize = 0;

/// Environment variables an agent must **not** inherit, and why.
///
/// Measured, not guessed. Running the end-to-end pane test from inside a Claude
/// Code session produced a working screen carrying this line:
///
/// ```text
/// ⚠ Transcript saving is off — inherited CLAUDE_CODE_CHILD_SESSION marker
/// ```
///
/// Transcript saving is **Channel D**, the JSONL tail — the one channel that
/// needs no hooks and no environment, the one `polis watch` is built on, and the
/// one that carries the session's own uuid in its filename (ADR-0096). A daemon
/// launched from inside an agent would have silently disabled it for every agent
/// it went on to start, and the map would have been permanently short a channel
/// for a reason nothing pointed at.
///
/// These are session **identity and IPC** variables only. Deliberately absent
/// are the `CLAUDE_CODE_*` names an operator sets on purpose —
/// `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_MAX_OUTPUT_TOKENS` and the like — so
/// this is a list and not a prefix rule: a prefix rule would throw away
/// configuration along with identity.
///
/// This is the mirror image of `polis_ingest::env::SUPPRESSED_ENV`, which names
/// what Polis declines to *set*; this names what it declines to *pass on*.
pub const INHERITED_AGENT_MARKERS: &[(&str, &str)] = &[
    ("CLAUDECODE", "says the parent is itself an agent"),
    (
        "CLAUDE_CODE_ENTRYPOINT",
        "how the parent session was started",
    ),
    (
        "CLAUDE_CODE_SESSION_ID",
        "the parent's session id, which would collide with the one Polis issues",
    ),
    (
        "CLAUDE_CODE_CHILD_SESSION",
        "turns transcript saving off, which is Channel D",
    ),
    (
        "CLAUDE_CODE_BRIDGE_SESSION_ID",
        "the parent's bridge session",
    ),
    (
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "an IPC channel back to the parent",
    ),
    ("CLAUDE_CODE_MESSAGING_TOKEN", "the secret for that channel"),
    ("CLAUDE_CODE_EXECPATH", "the parent's own executable"),
    ("CLAUDE_PID", "the parent's process id"),
];

/// Clears [`INHERITED_AGENT_MARKERS`] from this process's environment.
///
/// Called once by `polis-sessiond` **before it starts any thread**, which is
/// what makes it sound: `std::env::remove_var` mutates process-global state, and
/// the only safe moment for that is while the process is still single-threaded.
/// Doing it to the daemon rather than to each spawn is also the only place it
/// works — `alacritty_terminal`'s pty options can add to a child's environment
/// but cannot take anything away from it.
///
/// Returns the names actually removed, so a caller can say so rather than
/// leaving the operator to wonder.
pub fn disinherit_agent_markers() -> Vec<&'static str> {
    let mut cleared = Vec::new();
    for (name, _) in INHERITED_AGENT_MARKERS {
        if std::env::var_os(name).is_some() {
            std::env::remove_var(name);
            cleared.push(*name);
        }
    }
    cleared
}

/// What a pane's child is, and how big its screen starts out.
#[derive(Debug, Clone)]
pub struct PtySpec {
    /// The program, as the operator typed it.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// The child's working directory.
    pub cwd: Option<PathBuf>,
    /// Environment on top of the daemon's own, which is inherited.
    ///
    /// `alacritty_terminal` merges rather than replaces — verified by reading
    /// `tty/windows/conpty.rs::convert_custom_env`, which pulls
    /// `std::env::vars_os` in after the caller's own and lets the caller win.
    /// So `PATH` and `APPDATA` survive and `claude` can still find its config.
    pub env: Vec<(String, String)>,
    /// Rows.
    pub rows: u16,
    /// Columns.
    pub cols: u16,
}

/// Something that happened on the pty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyEvent {
    /// Raw bytes, exactly as the child wrote them.
    Output(Vec<u8>),
    /// The child is gone. Always the last event, always sent exactly once.
    Exited(Option<i32>),
}

/// What the owning thread is asked to do between reads.
#[derive(Debug)]
enum Task {
    Write(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Kill,
}

/// A live pty and the thread that drives it.
///
/// Dropping a `PtyHost` shuts the child down: an agent must not outlive the
/// thing that was supposed to be watching it, and "we forgot to call `close`" is
/// not an acceptable way to leak a `claude.exe`.
#[derive(Debug)]
pub struct PtyHost {
    tasks: Sender<Task>,
    poller: Arc<Poller>,
    pid: Option<u32>,
    image: PathBuf,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PtyHost {
    /// Starts `spec` on a new pty, delivering everything it does to `sink`.
    ///
    /// `sink` is called on the pty's own thread, never on the caller's, and is
    /// guaranteed to see exactly one [`PtyEvent::Exited`] as its last call.
    ///
    /// # Errors
    ///
    /// When the program is not on `PATH`, or the platform refuses a pty. Both
    /// are reported as one sentence for a status bar.
    pub fn spawn(
        spec: &PtySpec,
        label: &str,
        mut sink: impl FnMut(PtyEvent) + Send + 'static,
    ) -> Result<Self, String> {
        let resolved = exec::resolve(&spec.program, &spec.args)?;
        let options = Options {
            shell: Some(Shell::new(resolved.program.clone(), resolved.args.clone())),
            working_directory: spec.cwd.clone(),
            // The child's last words are usually the interesting ones.
            drain_on_exit: true,
            env: spec.env.iter().cloned().collect(),
            // `exec::resolve` has already quoted the program *and* the
            // arguments, which it must, because `alacritty_terminal` joins them
            // into one command line and never quotes the program itself.
            #[cfg(windows)]
            escape_args: false,
        };
        let mut pty = tty::new(&options, window_size(spec.rows, spec.cols), 0)
            .map_err(|error| format!("could not open a terminal for {}: {error}", spec.program))?;
        let pid = child_pid(&pty);

        let poller =
            Arc::new(Poller::new().map_err(|error| format!("could not open a poller: {error}"))?);
        register(&mut pty, &poller)
            .map_err(|error| format!("could not watch the terminal: {error}"))?;

        let (tasks, inbox) = crossbeam_channel::unbounded();
        let thread_poller = Arc::clone(&poller);
        let thread = std::thread::Builder::new()
            .name(format!("polis-pty-{label}"))
            .spawn(move || {
                let code = drive(pty, &thread_poller, &inbox, &mut sink, pid);
                sink(PtyEvent::Exited(code));
            })
            .map_err(|error| format!("could not start the terminal thread: {error}"))?;

        Ok(Self {
            tasks,
            poller,
            pid,
            image: resolved.image,
            thread: Some(thread),
        })
    }

    /// The child's process id, while the platform will say.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// The file that was actually started, before any `cmd.exe` wrapping.
    #[must_use]
    pub fn image(&self) -> &std::path::Path {
        &self.image
    }

    /// Sends bytes to the child. Queued, never blocking, never dropped.
    pub fn write(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let _ = self.tasks.send(Task::Write(bytes));
        self.wake();
    }

    /// Tells the pty its new size.
    ///
    /// Debouncing belongs to the caller — a dragged divider is a resize per
    /// frame, and `ResizePseudoConsole` makes the hosted app reflow its whole
    /// tree — but sending an unchanged size is free and is filtered here.
    pub fn resize(&self, rows: u16, cols: u16) {
        let _ = self.tasks.send(Task::Resize { rows, cols });
        self.wake();
    }

    /// Ends the child and waits for the thread to finish.
    ///
    /// Bounded by [`GRACE`] plus one `taskkill`, so a caller closing twenty
    /// panes is bounded too.
    pub fn shutdown(&mut self) {
        let _ = self.tasks.send(Task::Kill);
        self.wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// Interrupts a `wait` that is not otherwise going to return soon.
    fn wake(&self) {
        let _ = self.poller.notify();
    }
}

impl Drop for PtyHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Registers the pty with its poller.
///
/// The one `unsafe` in Polis, and it is a call rather than a block of raw FFI.
///
/// `EventedReadWrite::register` is an `unsafe fn` because on Unix it hands a
/// borrowed file descriptor to the poller, and the poller must not outlive it.
/// The obligation is discharged structurally: [`PtyHost`] owns the `Pty` and the
/// `Arc<Poller>` together, the `Pty` is moved into the thread that also holds
/// the only other `Arc`, and the `Pty` is dropped inside [`drive`] before that
/// thread returns — so the registration cannot outlive the source, whatever the
/// caller does.
///
/// The alternative was `portable-pty`, whose master is an ordinary blocking
/// `Read` and needs no poller — at the cost of roughly ten crates on a legacy
/// `winapi` 0.3 stack, in the one process that must never be flaky. The roadmap
/// asserted that neither option "forces `unsafe` into our crates"; that turned
/// out to be false for this one, and this comment is the correction.
#[allow(unsafe_code)]
fn register(pty: &mut Pty, poller: &Arc<Poller>) -> std::io::Result<()> {
    // SAFETY: see above — the `Pty` outlives its registration because both are
    // owned by the same thread and the `Pty` is dropped last.
    unsafe { pty.register(poller, Event::readable(READ_KEY), PollMode::Level) }
}

/// The pty's own thread: wait, act, drain, repeat.
///
/// Returns the child's exit code if the platform gave one.
fn drive(
    mut pty: Pty,
    poller: &Arc<Poller>,
    inbox: &Receiver<Task>,
    sink: &mut impl FnMut(PtyEvent),
    pid: Option<u32>,
) -> Option<i32> {
    let mut events = polling::Events::new();
    let mut buffer = vec![0u8; READ_CHUNK];
    let mut pending: VecDeque<u8> = VecDeque::new();
    let mut rows_cols = (0u16, 0u16);
    let mut exit_code = None;
    let mut child_gone = false;
    let mut killed = false;

    loop {
        let timeout = if pending.is_empty() {
            IDLE_WAIT
        } else {
            BUSY_WAIT
        };
        events.clear();
        if poller.wait(&mut events, Some(timeout)).is_err() {
            break;
        }

        while let Ok(task) = inbox.try_recv() {
            match task {
                Task::Write(bytes) => pending.extend(bytes),
                Task::Resize { rows, cols } => {
                    if rows_cols != (rows, cols) && rows > 0 && cols > 0 {
                        rows_cols = (rows, cols);
                        pty.on_resize(window_size(rows, cols));
                    }
                }
                Task::Kill => killed = true,
            }
        }

        // Writes before reads: a keystroke waiting on this loop is a person
        // waiting on this loop.
        while !pending.is_empty() {
            let (front, _) = pending.as_slices();
            match pty.writer().write(front) {
                Ok(0) | Err(_) => break,
                Ok(n) => drop(pending.drain(..n)),
            }
        }

        if drain(&mut pty, &mut buffer, sink) {
            child_gone = true;
        }

        if let Some(ChildEvent::Exited(status)) = pty.next_child_event() {
            exit_code = status.and_then(|s| s.code());
            child_gone = true;
        }

        if child_gone {
            // Whatever the child said on its way out is the part worth reading.
            drain(&mut pty, &mut buffer, sink);
            break;
        }
        if killed {
            break;
        }
    }

    // Closing the pseudoconsole is what actually ends the child. This drop
    // blocks until the output pipe is drained, which is deliberate: it is the
    // child's chance to flush.
    drop(pty);

    if killed && !child_gone {
        std::thread::sleep(GRACE);
        force_kill(pid);
    }
    exit_code
}

/// Reads everything available. Returns true when the master reached end of file.
fn drain(pty: &mut Pty, buffer: &mut [u8], sink: &mut impl FnMut(PtyEvent)) -> bool {
    loop {
        match pty.reader().read(buffer) {
            Ok(0) => return ZERO_READ_IS_EOF,
            Ok(n) => sink(PtyEvent::Output(buffer[..n].to_vec())),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
}

/// The last resort, for a child that ignored its console closing.
///
/// A Windows job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is the
/// textbook answer and cannot leak even if the daemon is itself killed — but it
/// costs either `unsafe` in the crate that runs child processes, or a
/// `windows-sys` dependency for one call. `taskkill` is in `System32`, kills the
/// tree, costs nothing, and gets most of the way. Revisit only if an orphan is
/// actually observed, and record the observation first.
fn force_kill(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    let mut command = if cfg!(windows) {
        let mut c = std::process::Command::new("taskkill");
        c.args(["/T", "/F", "/PID", &pid.to_string()]);
        c
    } else {
        let mut c = std::process::Command::new("kill");
        c.args(["-9", &pid.to_string()]);
        c
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Reaped, so the daemon does not accumulate zombies; the process itself is
    // bounded by the OS, and a failure means the child was already gone.
    let _ = command.status();
}

/// The size the pty is told about.
///
/// The pixel fields are a nominal cell, not the window's real metrics: they
/// reach the child as `TIOCSWINSZ`'s pixel dimensions, which Ink does not read,
/// and reporting the truth would mean the daemon knowing about fonts.
fn window_size(rows: u16, cols: u16) -> WindowSize {
    WindowSize {
        num_lines: rows.max(1),
        num_cols: cols.max(1),
        cell_width: 8,
        cell_height: 16,
    }
}

/// The child's process id, however this platform spells it.
fn child_pid(pty: &Pty) -> Option<u32> {
    #[cfg(windows)]
    {
        pty.child_watcher().pid().map(std::num::NonZeroU32::get)
    }
    #[cfg(not(windows))]
    {
        Some(pty.child().id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Collects everything a pty says, with a deadline, so a hung child fails
    /// the test rather than the suite.
    /// What a finished pty run produced.
    struct Ran {
        bytes: Vec<u8>,
        exited: bool,
        code: Option<i32>,
    }

    fn run(spec: &PtySpec, label: &str, deadline: Duration) -> Ran {
        let (tx, rx) = mpsc::channel();
        let host = PtyHost::spawn(spec, label, move |event| {
            let _ = tx.send(event);
        })
        .expect("spawn");
        let mut ran = Ran {
            bytes: Vec::new(),
            exited: false,
            code: None,
        };
        let started = std::time::Instant::now();
        while started.elapsed() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(PtyEvent::Output(chunk)) => ran.bytes.extend_from_slice(&chunk),
                Ok(PtyEvent::Exited(code)) => {
                    ran.exited = true;
                    ran.code = code;
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        drop(host);
        ran
    }

    fn echoing(text: &str, rows: u16, cols: u16) -> PtySpec {
        let (program, args) = if cfg!(windows) {
            (
                "cmd".to_owned(),
                vec!["/c".to_owned(), format!("echo {text}")],
            )
        } else {
            (
                "sh".to_owned(),
                vec!["-c".to_owned(), format!("echo {text}")],
            )
        };
        PtySpec {
            program,
            args,
            cwd: None,
            env: vec![("POLIS_PANE".to_owned(), "test".to_owned())],
            rows,
            cols,
        }
    }

    /// The whole architecture rests on this: bytes come out of a real pty, and
    /// the child's exit is observed rather than inferred from a zero-length read.
    #[test]
    fn a_child_runs_speaks_and_exits() {
        let ran = run(
            &echoing("polis-pty-lives", 24, 80),
            "lives",
            Duration::from_secs(20),
        );
        let text = String::from_utf8_lossy(&ran.bytes);
        assert!(text.contains("polis-pty-lives"), "{text:?}");
        assert!(ran.exited, "exactly one Exited event");
        assert_eq!(ran.code, Some(0), "and it carries the code");
    }

    /// The measurement in the module docs, asserted, so nobody re-adopts the
    /// roadmap's loop shape: on Windows a zero-length read is *not* end of file.
    #[test]
    fn a_zero_length_read_is_not_end_of_file_on_windows() {
        assert_eq!(ZERO_READ_IS_EOF, !cfg!(windows));
    }

    /// A pane must be able to talk to its child, which is the entire point.
    #[test]
    fn keystrokes_reach_the_child() {
        let (program, args) = if cfg!(windows) {
            ("cmd".to_owned(), vec!["/q".to_owned(), "/k".to_owned()])
        } else {
            ("sh".to_owned(), Vec::new())
        };
        let spec = PtySpec {
            program,
            args,
            cwd: None,
            env: Vec::new(),
            rows: 24,
            cols: 80,
        };
        let (tx, rx) = mpsc::channel();
        let mut host = PtyHost::spawn(&spec, "keys", move |event| {
            let _ = tx.send(event);
        })
        .expect("spawn");
        host.write(b"echo polis-typed-this\r".to_vec());

        let mut seen = String::new();
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(20) {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(PtyEvent::Output(chunk)) => {
                    seen.push_str(&String::from_utf8_lossy(&chunk));
                    // The echo of the command itself does not count; the output
                    // line does, and it is the second occurrence.
                    if seen.matches("polis-typed-this").count() >= 2 {
                        break;
                    }
                }
                Ok(PtyEvent::Exited(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        host.shutdown();
        assert!(
            seen.matches("polis-typed-this").count() >= 2,
            "the child echoed the command but never ran it: {seen:?}"
        );
    }

    /// The child's environment is the daemon's plus ours, not ours alone —
    /// `PATH` and `APPDATA` have to survive or `claude` cannot find its config.
    #[test]
    fn the_child_keeps_the_inherited_environment() {
        let (program, args) = if cfg!(windows) {
            (
                "cmd".to_owned(),
                vec!["/c".to_owned(), "echo [%POLIS_PANE%][%PATH%]".to_owned()],
            )
        } else {
            (
                "sh".to_owned(),
                vec!["-c".to_owned(), "echo \"[$POLIS_PANE][$PATH]\"".to_owned()],
            )
        };
        let spec = PtySpec {
            program,
            args,
            cwd: None,
            env: vec![("POLIS_PANE".to_owned(), "inherited-check".to_owned())],
            rows: 24,
            cols: 200,
        };
        let ran = run(&spec, "env", Duration::from_secs(20));
        let text = String::from_utf8_lossy(&ran.bytes);
        assert!(
            text.contains("inherited-check"),
            "our own variable: {text:?}"
        );
        assert!(
            !text.contains("[]"),
            "PATH came through empty, so the environment was replaced not merged: {text:?}"
        );
    }

    /// Dropping the host must end the child. An agent that outlives the thing
    /// watching it is the failure that makes people distrust a daemon.
    #[test]
    fn dropping_the_host_ends_the_child() {
        let (program, args) = if cfg!(windows) {
            ("cmd".to_owned(), vec!["/c".to_owned(), "pause".to_owned()])
        } else {
            (
                "sh".to_owned(),
                vec!["-c".to_owned(), "sleep 300".to_owned()],
            )
        };
        let spec = PtySpec {
            program,
            args,
            cwd: None,
            env: Vec::new(),
            rows: 24,
            cols: 80,
        };
        let (tx, rx) = mpsc::channel();
        let host = PtyHost::spawn(&spec, "orphan", move |event| {
            let _ = tx.send(event);
        })
        .expect("spawn");
        let pid = host.pid();
        assert!(pid.is_some(), "the platform should name the child");
        let started = std::time::Instant::now();
        drop(host);
        let took = started.elapsed();
        assert!(
            took < Duration::from_secs(5),
            "shutdown must be bounded, took {took:?}"
        );
        // The sink was called with Exited before the thread joined.
        let saw_exit = rx
            .try_iter()
            .any(|event| matches!(event, PtyEvent::Exited(_)));
        assert!(saw_exit, "the sink must always see exactly one Exited");
    }

    /// The finding this list exists for, asserted: a marker present in the
    /// daemon's environment must not reach an agent it starts.
    #[test]
    fn the_parents_session_markers_do_not_reach_the_child() {
        std::env::set_var("CLAUDE_CODE_CHILD_SESSION", "1");
        let cleared = disinherit_agent_markers();
        assert!(
            cleared.contains(&"CLAUDE_CODE_CHILD_SESSION"),
            "{cleared:?}"
        );
        assert!(std::env::var_os("CLAUDE_CODE_CHILD_SESSION").is_none());

        let (program, args) = if cfg!(windows) {
            (
                "cmd".to_owned(),
                vec![
                    "/c".to_owned(),
                    "echo [%CLAUDE_CODE_CHILD_SESSION%]".to_owned(),
                ],
            )
        } else {
            (
                "sh".to_owned(),
                vec![
                    "-c".to_owned(),
                    "echo \"[$CLAUDE_CODE_CHILD_SESSION]\"".to_owned(),
                ],
            )
        };
        let ran = run(
            &PtySpec {
                program,
                args,
                cwd: None,
                env: Vec::new(),
                rows: 24,
                cols: 80,
            },
            "disinherit",
            Duration::from_secs(20),
        );
        let text = String::from_utf8_lossy(&ran.bytes);
        assert!(
            !text.contains("[1]"),
            "the marker reached the child: {text:?}"
        );
    }

    #[test]
    fn every_suppressed_marker_carries_a_reason() {
        for (name, why) in INHERITED_AGENT_MARKERS {
            assert!(!name.is_empty());
            assert!(!why.is_empty(), "{name} needs a reason");
        }
    }

    #[test]
    fn an_unknown_program_fails_before_a_thread_is_started() {
        let spec = PtySpec {
            program: "polis-not-a-real-agent".to_owned(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            rows: 24,
            cols: 80,
        };
        let error = PtyHost::spawn(&spec, "missing", |_| {}).expect_err("must fail");
        assert!(error.contains("polis-not-a-real-agent"), "{error}");
    }
}
