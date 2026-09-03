# M7 — Terminal integration: Claude Code sessions inside the Polis window

*Written 2026-09-02, as a plan. See the status block below for what was built.*

> ## Status: built, and built inside out
>
> *Updated 2026-09-03. The plan below is preserved as written; this block records
> what was actually done and where the plan turned out to be wrong.*
>
> **M7 and M8 were built together, in the other order.** The ptys live in
> `polis-sessiond` from the first commit and the window is a thin client, which
> is what this document scheduled as M8 "later work". Three reasons, and the
> second is the one that decided it (ADR-0095):
>
> 1. A window that owns agents kills them when it closes, and a GPU driver reset
>    is not rare on Windows.
> 2. **The reader loop in *Threading — the house pattern, not tokio* does not
>    work.** On Windows `reader.read(&mut buf)` returns `Ok(0)` after **6.8 µs
>    having read nothing** — the master is a non-blocking drain, `Ok(0)` means
>    "no data", and end of file arrives through `next_child_event` instead.
>    Readiness needs a `polling::Poller` loop, and a process whose main thread
>    belongs to winit has nowhere natural to put one. A daemon *is* one, so the
>    inversion made the hard part easier rather than harder.
> 3. Doing it later means doing it twice.
>
> ### What shipped
>
> `polis-term` (pty, wire, transport, byte log, emulator, key table, widget,
> fonts), `polis-sessiond` (the daemon, `--status`, `--stop`, idle timeout),
> `polis-app/src/panes.rs` (dock, tab strip, focus arbitration, resize debounce),
> `Mode::Work`, `Repaint::Terminal`, `raw_input_hook`, and `polis work`.
> 85 tests across the two new crates; the whole workspace is green.
>
> Verified by hand, on this machine: Claude Code 2.1.248 renders in a pane
> (banner, box rules, `⎿`, spinner, `⏵⏵`) with the start-up `DSR`/`DA1` handshake
> answered in 1.6 s; a Polis window force-killed with `Stop-Process -Force` left
> its agent running, and the next `polis work` reattached and put the screen back;
> two windows can attach to the same pane; `--stop` takes every pane's child with
> it and leaves no orphan.
>
> ### Where this document was wrong
>
> * **The reader loop.** See above. The single most expensive line in the plan.
> * **"Neither option forces `unsafe` into our crates."** `EventedReadWrite::register`
>   is an `unsafe fn`. It is one `#[allow(unsafe_code)]` on one call with the
>   ownership argument written above it (ADR-0095).
> * **`windows-sys 0.59.0` (exact match).** `alacritty_terminal 0.26` wants
>   0.61.2 — which `notify` already pulls in, so the outcome is the same and the
>   reason is not.
> * **Named pipes vs Unix sockets, "the only genuinely platform-forked code in
>   M8".** There is none: it is loopback TCP with a token file, because `std` has
>   no named-pipe API and `CreateNamedPipeW` means FFI this workspace forbids
>   (ADR-0098).
> * **The font measurement was right and is easy to disprove wrongly.** Hack
>   *does* carry box-drawing, `seguisym.ttf` *does* close the gap at 16/16, and
>   `epaint`'s `Fonts::has_glyph` will tell you otherwise — it is a false negative
>   for every glyph sharing a face with `U+FFFD` (ADR-0097).
>
> ### What this document did not know
>
> * **`Ctrl+Alt` must never encode a control byte.** `AltGr` is reported as
>   Ctrl+Alt, and `AltGr+Q` is `@` on a German keyboard. The key table would have
>   sent `@` followed by `\x11` into every prompt containing an email address.
> * **A daemon inherits its parent's agent identity.** Started from inside a
>   Claude Code session it passes `CLAUDE_CODE_CHILD_SESSION` on, which turns
>   transcript saving — Channel D — off for every agent it starts (ADR-0098).
>
> ### Still ahead
>
> M7b's persisted dock width and collapsed state; M7c's selection, OSC 52 and the
> `polis doctor` glyph line (the report exists, the command does not print it
> yet); M7d's map↔pane correlation in both directions (`pane_for_session` is
> there and unused); M7e entirely. And the four ingest channels still start in the
> window rather than the daemon — until they move, a detached period records
> nothing, so "shut the lid for an hour and watch it play back" is not yet real.


Polis today watches Claude Code from the outside. `polis run -- claude`
(`polis-app/src/run.rs`) launches the agent as a **foreground child inheriting
the real console**, and spawns the map as a *silenced second process*. That
module's doc states the reasoning, and in the same breath names the thing this
milestone reverses:

> anything else is a pty emulation Polis has no reason to write.

The goal is one window: N Claude Code sessions running inside Polis, with the
city as one view among several — Polis as a full agentic coding surface, of
which the map is a part.

This is a genuine architecture change, not an extension. But it is far smaller
than it looks, because **the event plumbing is already complete and already
Claude-Code-shaped.** What is new is a PTY, a VT parser, and a text-grid widget.

## What already exists — reuse, do not rebuild

| Piece | Where |
|---|---|
| 4 live ingest channels (OTLP :4317, hook UDP :45177, fs-notify, JSONL tail) | `polis-ingest/src/{otlp,hook_listener,fswatch,transcript}.rs` |
| The 12-var telemetry block for a child agent | `polis_ingest::env::agent_env()` |
| 19 hook registrations into `.claude/settings.json` | `polis_ingest::env::hooks_block()` |
| PATH/`PATHEXT` resolution and Windows `.cmd` wrapping | `polis_app::setup::{which, command_for, quote_for_cmd}` |
| Single-writer world + `arc-swap` snapshot publish | `polis-world/src/{lib,snapshot}.rs` |
| Off-thread-work-into-UI pattern (named thread + `crossbeam` + per-frame `try_recv`) | `app.rs::spawn_load`, `session.rs::Picker::start` |
| Persisted UI state via a marker file, silent on failure | `polis-app/src/explain.rs` |
| Headless frame test with no window or adapter | `polis-app/tests/window_frame.rs` |

**What does not exist:** grep for `pty|conpty|vt100|alacritty|wezterm` across the
workspace returns zero hits.

## Decisions taken

1. **Real PTY panes.** The actual `claude` TUI over a ConPTY, rendered as an
   egui text grid. Full fidelity — slash commands, plan mode, permission
   prompts, `/resume` — permanently and for free, rather than reimplementing
   Claude Code's UI in egui and chasing it forever.
2. **Left dock, map right.** Sessions are the primary workspace, the city is the
   ambient companion. **The left pane is collapsible as a sidebar.**
3. **Scoped on its own**, not gated behind M3, though M7d absorbs M3's remaining
   hop because the two are the same wiring.

The alternative considered and rejected: driving `claude --print --output-format
stream-json --input-format stream-json` and rendering the conversation natively
in egui. It is genuinely reachable — the control protocol supports interactive
permission prompts, plan mode, interrupts and resume — but it means owning a
document renderer (streaming markdown, tool cards, inline diffs, subagent trees)
and then chasing upstream parity forever. Roughly 10–13 developer-weeks against
4–6, for a surface strictly less capable than the TUI it replaces.

## Adjacent, deliberately out of scope

`PermissionRequest` — already one of the 19 hooks Polis registers — can return a
`decision` object, and it fires ahead of the `permission_prompt` notification.
That would make the map's amber "waiting on you" pin **pressable**, for every
session on the machine including ones Polis did not launch. Complementary to
this milestone and worth a half-day feasibility experiment at some point.
**Not in M7.**

---

## What was measured before planning

Everything below that states a fact about egui internals, font coverage, or
Claude Code's glyph usage was read or measured, not assumed.

| Claim | How | Result |
|---|---|---|
| `ctx.egui_wants_keyboard_input()` == `memory.focused().is_some()` | `egui-0.36.1/src/context.rs:2983` | **True**, and clicking a `Button` does *not* set focus. A focused pane silently disables every map keybinding **with zero changes to `app.rs::read_keys`**. |
| egui-winit swallows Ctrl+C | `egui-winit-0.36.1/src/lib.rs:1021-1035` | **Confirmed landmine.** `is_copy_command` pushes `Event::Copy` and returns — the `Event::Key{C, ctrl}` is *never emitted*. Same for Ctrl+X and Ctrl+V. A naive pane cannot interrupt Claude Code. |
| A fix exists without forking | `eframe-0.36.1/src/epi.rs:279` | `fn raw_input_hook(&mut self, ctx, raw_input: &mut RawInput)` exists. `on_exit(&mut self)` at `:228` for our no-glow feature set. |
| Focus can lock Tab/arrows/Esc | `egui/src/memory/mod.rs:903` | `Memory::set_focus_lock_filter(id, EventFilter { tab, horizontal_arrows, vertical_arrows, escape })` — exactly the four a terminal must steal. Must be called **every frame** focus is held. |
| Galleys are memoized across frames | `epaint-0.36.1/src/text/fonts.rs:896,1068` | `GalleyCache` is a `nohash IntMap` with generation GC. An unchanged terminal row costs a hash lookup, not a reshape. |
| epaint does per-glyph font fallback | `epaint/src/text/font.rs:760-775` | True. `FontDefinitions::families` is an ordered fallback chain, and `Fonts::has_glyph` exists — usable as a `polis doctor` self-check. |
| **Glyph coverage of eframe's bundled fonts** | Parsed the `cmap` of all four TTFs in `epaint_default_fonts-0.36.1/fonts/` | Hack covers box-drawing (6/6 light, 4/4 round, 2/2 heavy, 2/2 double), blocks 6/6, eighth bars 4/4, arrows, bullets, powerline. **Missing from every bundled font:** braille `U+2800–28FF` (all 256), `✓ U+2713`, `✗ U+2717`, `✻ U+273B`, `✽ U+273D`, `✢ U+2722`, and **`⎿ U+23BF`**. |
| Claude Code actually draws those | Byte-scanned `claude.exe` (227 MB) for UTF-8 / UTF-16LE / `\uXXXX` forms | **All 14 probed codepoints present**, including `U+23BF` (the tool-result elbow, on *every* tool line), the `U+273B/273D/2722` spinner, the `U+23FA` bullet, and braille. |
| A zero-cost fix exists | Parsed `C:\Windows\Fonts\*.ttf` cmaps | **`seguisym.ttf` (Segoe UI Symbol, ships with every Windows since 7) covers 14/14 including all 256 braille cells.** Cascadia Mono/Code cover braille + box but **not** `⎿`, `✻`, `✗`. Consolas covers none of the gap. |
| `claude --session-id <uuid>` exists | `claude --help` (2.1.248) | Present, and **not gated on `--print`**. This is the pane↔session correlation, for free. |
| The window never starts `Ingest` today | grep across `polis-app/src` | Only `run.rs` and `commands.rs` construct `Ingest`; `app.rs` has none. M3's last hop is unlanded, and M7d is where it lands. |
| `claude` here is a native exe, but won't be everywhere | `where claude` | A winget `claude.exe`. npm installs give `claude.cmd`, which `CreateProcessW` refuses — `setup::command_for` already solves this for `std::process::Command` and the rule must be reused, not reimplemented. |
| CI is two-OS | `.github/workflows/ci.yml` | `ubuntu-latest` + `windows-latest`, `RUSTFLAGS: -D warnings`. `polis-term` must compile clean on Linux from the first commit. |

---

## Dependencies

**The grid: `alacritty_terminal = "0.26.0"`** (Apr 2026, Apache-2.0, MSRV 1.85 —
under the workspace's 1.95 floor). Nobody should write a VTE state machine
twice. Polis uses `Term`, the re-exported `vte::ansi::Processor`, and
`RenderableContent`.

Its 16 normal dependencies against this workspace's existing `Cargo.lock`:

| Already present at a satisfying version | Genuinely new |
|---|---|
| `bitflags` 2.13.1, `base64` 0.22.1, `libc` 0.2.189, `log` 0.4.34, `polling` 3.11.0, `parking_lot` 0.12.5, `regex-automata` 0.4.18, `unicode-width` 0.2.2, **`windows-sys` 0.59.0** (exact match), `serde` 1.0.229 | `vte`, `home`, `miow`, `piper` |

**Four new crates, all small.** `vte` arrives here and is re-exported — it must
**not** be pinned directly, or the graph carries two copies, which is the exact
failure the wgpu/winit ban in the root manifest exists to prevent.

**The PTY: decide in M7a's first hour, by spike.** Two options, and the cheap one
is tried first:

- **`alacritty_terminal::tty`** — already in the graph, **zero extra crates**.
  Windows support is `windows-sys 0.59` with the `console` feature, i.e. real
  ConPTY, the code Alacritty ships. Its master end is shaped for a `polling`
  event loop Polis does not run; the spike is whether `EventedReadWrite::reader()`
  is usable from a plain blocking thread.
- **`portable-pty = "0.9.0"`** (WezTerm's) — fallback if the spike fights us.
  Its master end is a plain `Read`/`Write` pair, exactly what a blocking reader
  thread wants, and it resolves `CreatePseudoConsole` at run time with a winpty
  fallback. It costs **~10 new crates on a legacy stack**: `winapi` 0.3,
  `bitflags` 1.3, `lazy_static`, `shared_library`, `winreg` 0.10, `serial2`,
  `filedescriptor`, `downcast-rs`, `nix` 0.28, `shell-words`, `anyhow`.

Record whichever wins, and why, in ADR-0095.

**`unsafe_code = "deny"` check:** neither option forces `unsafe` into our crates.
Both do their `CreateProcessW`/`CreatePseudoConsole` work internally behind safe
APIs. Unsafe inside a dependency is not this lint's business — which is exactly
why a hand-rolled ConPTY binding is off the table.

**No UUID crate.** `uuid` is only transitive here. A v4-shaped session id is ~15
lines from `(process id, pane ordinal, SystemTime nanos)` with the version nibble
and variant bits set. It must be unique on one machine, not unpredictable. This
matches the house call that wrote out simplex noise rather than take `noise`
(ADR-0050) and `LogicalPath::layout_seed` rather than take `DefaultHasher`.

**No `unicode-width`.** `alacritty_terminal`'s grid already marks double-width
cells (`cell::Flags::WIDE_CHAR` / `WIDE_CHAR_SPACER`). Read the flag rather than
recompute the width — that also guarantees our layout agrees with the emulator's.

**Rejected: `egui_term`.** Explicitly untested on Windows, pins its own
`alacritty_terminal` (a resolver conflict waiting to happen), and hands us
someone else's abstraction plus a fork burden for roughly the code we would write
anyway. Worth reading as a reference for its input table.

---

## Architecture

### New crate: `polis-term`

Ninth workspace member, depending on `eframe` the way `polis-render` does. Four
reasons, in order of weight:

1. **The tests are the argument.** A terminal emulator's test is "feed these
   bytes, assert this grid" — pure, headless, sub-millisecond, a perfect fit for
   the golden-file style PRD §16 mandates. Put it in `polis-app` and every grid
   assertion sits behind a 12 800-line crate.
2. **The dependency fence.** The two new pins land in exactly one manifest.
   `alacritty_terminal` breaks its API between minors with some regularity; the
   blast radius must be one member.
3. **Rebuild cost.** `polis-app` is edited every five minutes; `polis-term` will
   be stable within a week and then compiled once.
4. **PRD §14's list is a list of roles.** `polis-app` is "winit shell, UI
   overlay, config, CLI". A VTE state machine is none of those.

The split is at the **product** boundary, not the technology boundary:

- **`polis-term`** knows about one pane: spawn a child on a PTY, parse its bytes,
  draw one grid into one `Rect`, turn egui events into terminal bytes. It knows
  nothing about Polis, sessions, the map, or tabs.
- **`polis-app/src/panes.rs`** (new) knows about the Polis window: how many
  panes, which is focused, where the dock sits, which pane owns which
  `session_id`, what the tab strip says, when to kill everything.

```
polis-term/src/
  lib.rs      module docs, PRD/ADR references, `pub use`; the `Pane` type
  pty.rs      PtyProcess: openpty, spawn, reader thread, writer, resize, kill
  emu.rs      Emulator: alacritty Term behind a Mutex + PaneListener
  input.rs    egui Event -> Vec<u8>, the key table, bracketed paste
  widget.rs   paint one grid into a Rect; cursor; selection; scrollback
  font.rs     the terminal family, the fallback chain, the coverage report
polis-term/tests/
  grid.rs     golden grids over recorded Claude Code byte streams
  keys.rs     the key table, asserted byte-for-byte
```

### Threading — the house pattern, not tokio

Per `polis-app/src/lib.rs`: winit owns main, there is no `#[tokio::main]`
anywhere, and background work is named `std::thread` + `crossbeam-channel`.
`polis-app` does not depend on tokio and must not start.

**One reader thread per pane**, named `polis-term-read-{n}`:

```rust
let mut buf = [0u8; 64 * 1024];
loop {
    match reader.read(&mut buf) {
        Ok(0) | Err(_) => break,           // EOF == the child's console closed
        Ok(n) => {
            emulator.feed(&buf[..n]);      // locks Term, parses, unlocks
            generation.fetch_add(1, Ordering::Release);
            ctx.request_repaint();         // egui::Context is Clone + Send
        }
    }
}
```

**There is no byte channel to the UI thread.** The reader parses directly into
the shared `Term` and then wakes the frame; the UI thread never sees a byte. A
4 MB `cat` costs the frame one lock acquisition. The alternative — a channel of
`Vec<u8>` drained in `ui()` — puts an unbounded, adversarially-sized parse inside
the 16.6 ms budget.

`Emulator::feed` chunks at 16 KiB so a large burst cannot hold the lock across a
frame:

```rust
for chunk in bytes.chunks(16 * 1024) {
    let mut term = self.term.lock().expect("poisoned");
    self.processor.borrow_mut().advance(&mut *term, chunk);
}
```

Damage tracking is one `Arc<AtomicU64>` generation bumped per `feed`. The UI
compares it against what it drew last frame; equal means the pane is not a
repaint reason. Three lines, and exactly the granularity `Repaint` needs.
`alacritty_terminal`'s finer `TermDamage` is a later optimisation.

### `PaneListener` is load-bearing, not boilerplate

```rust
impl EventListener for PaneListener {
    fn send_event(&self, event: Event) {
        match event {
            // MANDATORY. Ink queries cursor position (DSR 6) and device
            // attributes (DA1) during start-up and BLOCKS on the answer. A
            // terminal that does not reply is a terminal Claude Code hangs in.
            Event::PtyWrite(text) => { /* write_all to the master */ }
            Event::Title(t) | Event::ResetTitle => { /* -> the tab label */ }
            Event::Bell => { /* -> an attention mark, not a beep */ }
            Event::ClipboardStore(_, s) => { /* OSC 52 -> ctx.copy_text(s) */ }
            Event::Wakeup => self.ctx.request_repaint(),
            _ => {}
        }
    }
}
```

The `PtyWrite` arm is the single most likely thing to forget and the single most
likely cause of "the pane opens and Claude Code never draws anything".

### Spawn details that bite

- **`drop(pair.slave)` immediately after spawn.** An open slave keeps the
  master's read end alive forever and the reader thread never sees EOF.
- **The `.cmd` rule.** A `.cmd`/`.bat` is a script, not an image, and
  `CreateProcess` refuses it. `setup::command_for` already solves this with
  `cmd.exe /d /s /c` and `quote_for_cmd`. **Extract that rule** as
  `setup::cmd_shim(program, args) -> Option<(OsString, Vec<OsString>)>` so
  `polis-term` never needs to know about `cmd.exe` and `quote_for_cmd`'s existing
  tests stay the single source of truth.
- **The environment.** `agent_env(endpoint)`'s 12 vars, plus
  `TERM=xterm-256color` (ConPTY does not set it; Ink's colour detection reads
  it), `COLORTERM=truecolor` (without it, diffs degrade to 16 colours), and
  `POLIS_PANE=<uuid>` (diagnostic only). **`TERM_PROGRAM` is deliberately
  unset** — Claude Code branches on it, and claiming to be an unknown terminal is
  a worse bet than claiming nothing until measured. Verify the PTY crate
  inherits the parent environment; `PATH` and `APPDATA` must survive or `claude`
  cannot find its own config.

### Resize

Call `master.resize()` **and** `Term::resize()`, in that order, only when the
value actually changed.

- **Debounce 100 ms.** A dragged divider produces a resize per frame;
  `ResizePseudoConsole` forces the hosted app to reflow and Ink repaints its
  whole tree. Without a debounce, dragging makes Claude Code strobe and burns
  real CPU in the child. Apply immediately on drag release.
- **Floor the columns.** Claude Code's box-drawn input frame collapses below
  ~60 columns and its diffs below ~80. `TerminalConfig::min_cols` (default 80)
  is enforced by shrinking the font, not by lying about the size; when even that
  fails the pane says "too narrow — widen the window or close the rail" rather
  than showing a broken TUI. **This is the main cost of the left-dock layout and
  it must be respected.**

### Process lifetime

`eframe::App::on_exit` runs `Panes::shutdown_all()`, per pane:

1. **Drop the master PTY handle.** Closing the ConPTY pulls the console out from
   under the child. For a well-behaved Node app this alone suffices, and it is
   the graceful path — Claude Code gets to flush its transcript.
2. **Poll `try_wait()` for up to 2 s** (bounded; `on_exit` must not hang the
   closing window).
3. **`Child::kill()`** — the direct child only.
4. **Still alive after 1 s more: `taskkill /T /F /PID <pid>`**, null stdio, not
   waited on.

**Why not a Windows job object.** `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is the
textbook answer and cannot leak even if Polis is itself killed — but it costs
either `unsafe` in the crate that runs child processes (exactly the `#[allow]`
you do not want) or a new `windows-sys` dependency. Step 4 gets ~95 % of it with
a tool in `System32`, zero deps, zero unsafe, one line. **Revisit only if M7a
observes an orphan, and record the observation before adding the dependency.**

A pane whose child exits **keeps its final grid on screen**, greys the tab and
shows the exit code. A pane that vanishes takes the error message with it, and
the error message is why you looked.

---

## The idle budget — the constraint most likely to be violated

PRD §13.1 requires **<2 % of one core idle** and adds "this thing runs all day".
`app.rs`'s `enum Repaint { Idle, Loading, Playing, Animating, KeyHeld }` enforces
it, and the status bar names whichever cause is keeping the window awake.

- Each reader thread calls `ctx.request_repaint()` **only when the generation
  changed**. Never poll. A silent pane calls it zero times and egui sleeps.
- Add `Repaint::Terminal` with a `label()` of `"terminal"`, so the status bar
  stays honest.
- **A 30 Hz ceiling on terminal-driven repaints:** when a pane is chatty, use
  `ctx.request_repaint_after(Duration::from_millis(33))`. Claude Code's spinner
  runs at 8–12 Hz, so 30 Hz is invisible; the ceiling exists for the
  `cat`-a-large-file case, where uncapped repaint pins a core. At 30 fps × ~1.5 ms
  that is ~4.5 % of one core — **above the idle budget, and correctly so: an
  agent producing output is not idle.** Say it in the status bar rather than
  hiding it.
- **The cursor does not blink by default.** A blinking cursor is a permanent
  2 Hz wake-up and would be the single largest idle regression in the product.
- **Exactly one pane is drawn at full size.** Unfocused panes are a tab label and
  a one-line status, not a rendered grid. Largest single lever, and also the
  right UX.

---

## Focus arbitration

The mechanism already exists and needs **no change to `app.rs::read_keys`**.

```rust
let resp = ui.interact(rect, pane_id, Sense::click_and_drag());
if resp.clicked() { resp.request_focus(); }
if resp.has_focus() {
    ui.memory_mut(|m| m.set_focus_lock_filter(pane_id, egui::EventFilter {
        tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true,
    }));
    // ... consume events
}
```

The instant a pane has focus, `egui_wants_keyboard_input()` is true, `read_keys`
returns `Keys::default()`, and `t`, `i`, `h`, `s`, `r`, `p`, `n`, `[`, `]`,
`.`, `,`, Space and the arrows all go silent for the map. The `EventFilter` stops
egui itself stealing Tab/arrows/Esc for focus navigation. Clicking the map
surrenders focus and the map keys return.

What that does not give you is a way to reach a map key without clicking, so a
small reserved band handled in `panes.rs` **before** the pane consumes events:

| Chord | Action |
|---|---|
| `Ctrl+Alt+1..9` | focus pane N |
| `Ctrl+Alt+M` | focus the map (surrender pane focus) |
| `Ctrl+Alt+T` | new pane |
| `Ctrl+Alt+W` | close the focused pane (confirm if the child is alive) |
| <code>Ctrl+&#96;</code> | collapse / expand the sidebar |
| `F6` | cycle map → pane 1 → pane 2 → … → map |

`Ctrl+Alt` is used by neither Claude Code nor any shell. **Not `Esc`** — Esc
belongs to Claude Code (interrupt / clear input) and stealing it would be the
most annoying possible choice.

### Ctrl+C — a confirmed defect-in-waiting

`egui-winit` **never emits** `Event::Key { key: C, modifiers: ctrl }`. Verified at
`egui-winit-0.36.1/src/lib.rs:1024-1026`: `is_copy_command` pushes `Event::Copy`
and returns. Ctrl+C is the key that interrupts Claude Code. Untreated, **the
operator cannot stop a runaway agent from inside Polis** — a safety property, not
a convenience.

Fix: `PolisApp::raw_input_hook` (`eframe-0.36.1/src/epi.rs:279`), which runs
before egui processes the frame's input, applying the Windows Terminal rule —
copy when there is a selection, interrupt when there is not:

```rust
fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
    let Some(_pane) = self.panes.focused_with_no_selection() else { return };
    for event in &mut raw.events {
        match event {
            egui::Event::Copy => *event = key(egui::Key::C, ctrl()),
            egui::Event::Cut  => *event = key(egui::Key::X, ctrl()),
            _ => {}
        }
    }
}
```

`Event::Paste(String)` is **left alone** and consumed as a bracketed paste —
pasting is what Ctrl+V means. `Ctrl+Shift+C` copies explicitly. **Known
limitation to document, not fix:** Claude Code's Ctrl+V image paste cannot work,
because `egui::Event::Paste` carries only text. Dropping an image file onto a
pane and inserting its path is the M7e workaround.

### The key table

`fn to_bytes(event, mode: TermMode) -> Option<Vec<u8>>`, asserted byte-for-byte
in `polis-term/tests/keys.rs`.

| Input | Bytes | Note |
|---|---|---|
| `Event::Text(s)` | `s.as_bytes()` | already IME/dead-key composed by winit; **never reconstruct printables from `Key`** |
| Enter | `\r` | not `\n` |
| **Shift+Enter** | `\x1b\r` | what `/terminal-setup` installs. **A pane gets multi-line input with no setup step, which no external Windows terminal gives you.** A genuine product win — put it in the help sheet. |
| **Ctrl+C** | `\x03` | via `raw_input_hook`; copies instead when there is a selection |
| Ctrl+D/L/R/U/W/Z | `\x04 \x0c \x12 \x15 \x17 \x1a` | general rule: `letter.to_ascii_uppercase() - 0x40` |
| Esc | `\x1b` | requires `EventFilter::escape` |
| Tab | `\t` | requires `EventFilter::tab` |
| **Shift+Tab** | `\x1b[Z` | CBT. **Claude Code cycles permission modes with this.** Very visible when wrong. |
| Backspace | `\x7f` | DEL, not BS. Ctrl+Backspace → `\x17` |
| Delete | `\x1b[3~` | |
| Arrows | `\x1b[A/B/C/D`, **or `\x1bOA/B/C/D` under `TermMode::APP_CURSOR`** | reading the mode is mandatory; Ink enables DECCKM |
| Ctrl/Alt+Arrow | `\x1b[1;5A` / `\x1b[1;3A` | CSI modifier params 5=ctrl, 3=alt, 2=shift |
| Home / End | `\x1b[H` / `\x1b[F` (`\x1bOH`/`\x1bOF` in app mode) | |
| PageUp / PageDown | `\x1b[5~` / `\x1b[6~` | **Shift+PageUp/Down is ours** (scrollback), not forwarded |
| F1–F4 | `\x1bOP/Q/R/S` | |
| F5–F12 | `\x1b[15~ 17~ 18~ 19~ 20~ 21~ 23~ 24~` | |
| Alt+letter | `\x1b` + the letter | |
| `Event::Paste(s)` | `\x1b[200~` + `s.replace('\n', "\r")` + `\x1b[201~` **when `TermMode::BRACKETED_PASTE`**, else raw | **Must check the mode.** Without it a pasted multi-line prompt is submitted line by line — destructive and obvious. Also strip `\x1b[201~` from the payload so a hostile clipboard cannot close the bracket early. |
| Mouse | not forwarded in v1 | drag = selection. M7e forwards SGR-1006 when `TermMode::MOUSE_*` is set and Alt is not held (Alt = "give me selection anyway", the standard override). |

---

## The widget

Cell metrics, computed once per (font size, ppp) and cached:

```rust
let font   = FontId::new(size, FontFamily::Name("polis-term".into()));
let cell_w = ctx.fonts(|f| f.glyph_width(&font, 'M'));
let cell_h = ctx.fonts(|f| f.row_height(&font)).round();  // rounded so N rows land on pixels
```

`rows = floor(rect.height() / cell_h)`, `cols = floor(rect.width() / cell_w)` —
and those two numbers, not the pixel rect, are what `resize` is told.

**One galley per attribute run, with the pen reset at every run boundary.** Not
per cell (5 400 layout calls per frame), and not per row (drifts the moment a
glyph resolves to a fallback face with a different advance, and a terminal that
drifts is a broken terminal). Break a run when fg, bg, or
`Flags & (BOLD|ITALIC|UNDERLINE|STRIKEOUT|INVERSE|HIDDEN)` changes — **and emit a
single-char galley whenever the cell's char is not exactly one cell wide**, via a
memoised oracle (ASCII from a 128-entry table, everything else measured once into
an `ahash` map — a hot lookup never iterated for output, per the warning above
the `ahash` pin).

In practice Claude Code's screen is ASCII plus box-drawing plus a handful of
symbols; box-drawing *is* in Hack at monospace advance, so it stays on the fast
path. Only the ~5–20 fallback glyphs per screen become single-char galleys, each
placed at `origin.x + col * cell_w` and centred in its cell. Because every run's
origin is computed from its column index, **drift cannot accumulate**.

Paint order per row:

1. **Backgrounds** — merge adjacent same-bg cells into one `Shape::rect_filled`
   at exactly `[col_start*cell_w, row*cell_h]..[col_end*cell_w, (row+1)*cell_h]`.
   Drawn by us, **not** via `TextFormat::background`, because epaint sizes those
   to the glyph and a terminal's background must tile with no seams.
2. **Selection** — one rect per span, from `RenderableContent::selection`.
3. **Text** — the run galleys via `painter.galley`.
4. **Cursor** — filled rect when focused, hollow stroke when not. No blink.

**Estimated cost for a focused 45×120 pane: ~1–2 ms/frame**, dominated by
tessellation (~4 000 glyphs → ~16 k vertices), one draw call, ~0 re-shapes after
frame 1 thanks to `GalleyCache`. **These are estimates and must be replaced by
measured numbers in ADR-0095 the day M7a runs** — this workspace's culture is
measured numbers.

Scrollback: `term.scroll_display(Scroll::Delta(n))` from `raw_scroll_delta.y /
cell_h`, plus Shift+PageUp/Down. `display_offset > 0` shows a "scrolled back N
lines — press End to return" chip and freezes autoscroll; any keystroke that
produces bytes snaps to the bottom.

### The font — measured, and the surprise

`polis_term::font::install(ctx)`, called from `app.rs::theme()`:

1. Start from `FontDefinitions::default()`.
2. Register `FontFamily::Name("polis-term")` = `["Hack", <system symbol font>,
   "NotoEmoji"]`.
3. Load the symbol font from disk at run time, best-effort:
   - Windows: `%WINDIR%\Fonts\seguisym.ttf` — **measured to cover 14/14 of the
     gap, including all 256 braille cells.**
   - Linux: `DejaVuSans.ttf` / `NotoSansSymbols2-Regular.ttf`.
   - macOS: `/System/Library/Fonts/Apple Symbols.ttf`.
4. A missing file is not an error — it degrades to replacement characters and is
   reported.

**Cost: zero bytes of binary, zero licence question** — we read a font the OS
installed for its own use; we do not redistribute it. **Bundling Cascadia Mono
instead would cost 363 KiB and — measured — still miss `⎿ U+23BF`, `✻ U+273B`
and `✗ U+2717`, all three of which are in `claude.exe`.** Bundling is strictly
worse here. Do not do it.

Add to `polis doctor`: a `Fonts::has_glyph` sweep over the table of glyphs Claude
Code is known to draw, printing `terminal glyphs  47/47 (Segoe UI Symbol)` or
naming exactly which are missing. That table *is* the evidence in ADR-0097, and
it turns a cosmetic mystery into a one-line diagnosis.

---

## Window layout — collapsible left sidebar

Panels stack outside-in in `app.rs::draw_scene`, using egui 0.36's unified
`egui::Panel::{bottom,top,left,right}` API. The dock is inserted **after
`polis-title` and before `polis-rail`**, so the map keeps
`available_rect_before_wrap()`:

```
Panel::bottom("polis-status")        26 px
Panel::bottom("polis-transport")     88 px, replay only
Panel::top("polis-title")            30 px
Panel::left("polis-terminals")       resizable, collapsible   <- new
Panel::right("polis-rail")           370 px default
  -> the rest is the map
```

Inside the dock: a **tab strip** (24 pt) and **one pane** at full dock width. Not
a tiling splitter — a tab strip is ~80 lines, a splitter with drag handles, focus
follow and persistence is ~600, and every one of them is on the critical path to
a demo. The map is already the parallelism view (PRD §11): you watch several
agents on the *map* and read *one* terminal.

**The left dock's one real cost is columns.** `min_cols` (default 80) times the
cell width sets a floor on the dock, and the map gets what is left. The dock's
`size_range` is therefore `(80 * cell_w)..=(available_width * 0.7)`, and the
default width is whatever 100 columns needs. If the window is too narrow to give
both the dock its floor and the map something usable, the rail auto-closes first.

**Collapsed**, the dock becomes a thin rail (~28 px) of one chip per session
showing its attention state, so a waiting session stays visible with the sidebar
shut. Toggle is <code>Ctrl+&#96;</code>; bare backtick is unbound today but must
reach the terminal when a pane has focus. Width and collapsed state persist via
the `explain.rs` marker-file pattern in `Config::default_state_dir()`
(`%LOCALAPPDATA%\polis`), **silent on failure**.

Each tab shows the pane ordinal, its title (OSC 2 from `PaneListener`), a status
dot in `palette::status(ThreadStatus)` colours, and — once M7d lands — **the
district its agent is currently working in**, from the world snapshot via the
session id. That last one is what makes the dock feel like part of Polis rather
than a terminal bolted on.

**M7e** adds `Ctrl+Alt+\` = split, enabled only when the dock can measure two
usable panes — offered when it fits, hidden when it does not.

### `Mode`, not `View`

**Not a `View` variant.** `View { Map, Tree }` is "which of two co-equal
renderings of one data structure is in the central area" (PRD §12, and `ui.rs:45`
says so). Terminals are not a rendering of the world; they coexist with whichever
rendering is up, and `View::swapped()` would become meaningless.

**A new `Mode` variant**, because `Mode` is "what the window was started for":

```rust
/// `polis work` — the map, live ingest, and agents running in panes (PRD §15 M7).
///
/// The difference from `Mode::Map` is not cosmetic: this is the only variant
/// that starts an `Ingest` inside the window, and the only one that owns child
/// processes. `Mode::Map` stays exactly what it is — a city with no receiver
/// and nothing to kill on the way out.
Work { repo: PathBuf, panes: usize },
```

Dock visibility is `Overlay::terminals: bool` (default true in `Work`, false
elsewhere), so a `Replay` session can still open a scratch pane — there is no
reason to forbid it and forbidding it would be a rule to explain.

CLI: `polis work [--panes N] [-- <program> args…]`, defaulting to `claude`.
**`polis run` is untouched.**

---

## Session ↔ pane correlation

**Polis issues the session id.** `panes.rs` generates a v4-shaped uuid *before*
spawning, appends `--session-id <uuid>` when the program is `claude` (matched the
way `run.rs::resolve_agent` matches `DEFAULT_AGENT`), and stores it on the
`PaneEntry`. Every channel then carries it:

- **Channel A (OTLP)** — the `session.id` resource/log attribute.
- **Channel B (hooks)** — `session_id` in the payload `hook_listener` already parses.
- **Channel D (transcript)** — the file is literally
  `~/.claude/projects/<slug>/<uuid>.jsonl`, so `SessionIndex` finds it by name
  with no heuristic at all.

`Panes::pane_for_session(&SessionId) -> Option<usize>` is a `BTreeMap` lookup, and
clicking a cloud focuses its pane in one line. Nothing inferred, nothing timed,
nothing raced.

**Explicitly rejected: threading a pane id through `polis-hook`.** Tempting
because `polis-hook` already reads an env var (`resolve_endpoint()` checks
`POLIS_HOOK_ENDPOINT` at `main.rs:162`), so one more `var_os` is sub-microsecond
against a 3 ms p99 budget. **The env read is not the problem.** The hook ships an
8-byte header and opaque bytes (`main.rs:11-18`), so a pane id would go *into the
wire format* — a new field, a version bump, a matching `hook_listener` change,
and a re-derived compile-time assertion — to the one binary whose contract is
"never blocks the agent, never exits non-zero, no deps beyond `std`", in exchange
for what `--session-id` gives free. **Do not touch `polis-hook`.**

**Also rejected: a hook listener port per pane.** It needs zero hook changes
(each pane gets its own `POLIS_HOOK_ENDPOINT`; the receiving socket identifies
the pane) and is elegant — but it needs N sockets and N threads in `Ingest` and
multiplies the `AddrInUse` singleton logic ADR-0026 carefully established, for
nothing over `--session-id`. Record it in ADR-0096 as the fallback if
`--session-id` is ever removed.

**Fallback for a non-`claude` pane:** reuse
`run.rs::transcript_from_this_run`'s rule — snapshot `transcripts_now()` before
the spawn, then claim the transcript that did not exist before *and* whose `cwd`
matches. With one pane it is exact; with several started in the same second it
can mis-assign, so the pane shows "session not identified" rather than guessing.
**A false positive here points a cloud at the wrong terminal, which is worse than
pointing at none.**

---

## Staged delivery

Each stage ends in something you can put in a GIF, as `docs/replay/README.md`
already does for M1/M2.

### M7a — One pane, one agent, inside the window — **8–12 dev-days**

`polis-term` crate; the PTY spike and decision; `PtyProcess`; `Emulator` with
`PaneListener` **including `PtyWrite`**; the widget (runs, backgrounds, cursor;
no selection, no scrollback UI); the key table including `raw_input_hook` and
Shift+Enter; the font family with the `seguisym.ttf` fallback; `polis work`
opening one pane in the left dock beside the static map.

Breakdown: crate + pins 0.5 · pty 1.5 · emulator 1.0 · widget 2.0 · input 1.5 ·
app wiring 1.0 · font 0.5 · **debugging a real Claude Code session under a
non-Windows-Terminal ConPTY host 2.0–4.0**. That last line is where the time
actually goes; pretending otherwise is how this slips.

**Build the PTY byte-log first, not last** — capture raw master output to a file
so a corrupt screen replays into `tests/grid.rs` rather than being reproduced by
hand. It is the fixture those tests need anyway.

**Demo:** `polis work` — the city on the right, Claude Code in the dock on the
left, Ctrl+C interrupts it, Shift+Enter writes a second line, the spinner and the
`⎿` elbows render.

**Gate:** a 60-second recorded session where the grid never corrupts on resize,
and `POLIS_DEBUG_FRAMES` shows p99 under 16.6 ms with the pane drawing.

### M7b — Several panes, tabs, collapsible sidebar, session ids — **4–6 dev-days**

`Panes` with a `Vec<PaneEntry>`; the tab strip; the collapse toggle and the
collapsed chip rail; persisted width and collapsed state; `Ctrl+Alt+1..9/T/W`,
<code>Ctrl+&#96;</code>, `F6`; `--session-id` generation and `pane_for_session`;
per-pane exit status kept on screen; `on_exit` shutting all of them down; the
too-narrow message; resize debounce.

**Demo:** three agents in three tabs in one window, each with a distinct session
id in the status bar; the sidebar collapsing to a chip rail and back; the window
closing without leaving a `claude.exe` behind — checked in Task Manager, on
camera.

### M7c — Scrollback, selection, copy, and the honest status bar — **3–4 dev-days**

10 000-line scrollback with Shift+PageUp and the scrolled-back chip; drag
selection via `alacritty_terminal::selection::Selection`; Ctrl+C-with-selection
copies and Ctrl+Shift+C always copies; OSC 52; `Repaint::Terminal` and the 30 Hz
ceiling with the reason shown; the `polis doctor` glyph-coverage report.

**Demo:** scroll back through a long session, select a stack trace, paste it into
an editor. Status bar reads `terminal` while output flows and `idle` two seconds
after it stops.

### M7d — The dock and the map are the same thing — **4–6 dev-days**

Where the product thesis arrives, and most of it is M3 work that has to happen
anyway: `Mode::Work` starts `Ingest` inside the window (the `world thread` row
`lib.rs`'s threading table already promises), the drain feeds `World`, and the
`session_id` correlation lights up both directions — a tab shows the district its
agent is in; clicking a cloud focuses that agent's pane; a pane whose agent hits
`PermissionRequest` flashes its tab in `palette::needs_decision()`.

**Demo:** three agents working; one stops for a permission prompt; its cloud goes
amber on the map, its tab flashes, one click puts you in the right terminal.
**That is the milestone that justifies the entire feature.**

### M7e — The sharp edges — **3–5 dev-days**

Split when the dock can measure two usable panes; SGR-1006 mouse forwarding gated
on `TermMode`; confirm-on-close when children are alive
(`ViewportCommand::CancelClose`); dropped image files insert their path; IME
preedit rendering; per-pane font size.

**Total: 22–33 dev-days (4.5–6.5 developer-weeks)**, and M7a alone is shippable
and demonstrable. Terminal emulation is famous for the last 10 % costing as much
as the first 90 %; this estimate assumes that and is still the honest number, not
the optimistic one.

### What M7 must leave in place for M8

Four constraints, none of which costs M7 anything, and which together are the
difference between M8 being 4–6 days and being a rewrite:

1. **Write the per-pane raw byte log from day one.** M7a already plans it as the
   fixture `tests/grid.rs` needs — just make it permanent rather than test-only.
   In M8 it becomes pane history, and pane history is what makes reattach work.
2. **The UI reads only `ScreenSnapshot`, never the `Term` directly.** Already the
   M7 design; this note records *why* it matters — that boundary is where the
   process split lands.
3. **PTY spawn / resize / kill live behind one type**, not scattered across
   `panes.rs`, so M8 swaps an implementation rather than hunting call sites.
4. **`Ingest` construction in `Mode::Work` stays a single call site**, so it can
   be replaced by "connect to the daemon" in one edit.

---

## Risks, ranked

1. **ConPTY resize is most likely to look broken.** `ResizePseudoConsole` forces
   the hosted app to reflow and Ink repaints its whole tree; a dragged divider is
   a resize per frame. *Mitigation:* the 100 ms debounce, applied immediately on
   release. *Residual:* ConPTY has a long-standing habit of duplicating or
   clipping the last line across a resize; the ugly-but-working escape hatch is
   re-sending `\x0c` after a settled resize.
2. **Claude Code's TUI under a non-Windows-Terminal ConPTY host.** Unknowns:
   whether it uses the alternate screen (it appears not to for the main REPL —
   it repaints inline, which is why scrollback matters), whether it emits DECSET
   2026 synchronized output that must at least be ignored cleanly, and whether it
   blocks on a DA1/DSR reply at start-up. The third is the dangerous one and is
   why `Event::PtyWrite` is called out as mandatory.
3. **Glyph coverage — measured, not speculative.** `⎿ U+23BF`, `✻ U+273B`,
   `✽ U+273D`, `✢ U+2722`, `✗ U+2717` and the whole 256-cell braille block are in
   `claude.exe` and in **none** of eframe's four bundled fonts. Untreated, every
   tool-result line and every spinner frame is a replacement character.
   *Mitigation:* the `seguisym.ttf` fallback, measured to cover all of them at
   zero binary cost. *Residual:* on Linux the equivalent is not guaranteed
   present, so CI's ubuntu leg asserts that `install()` degrades without
   panicking, not that coverage holds; `polis doctor` reports the truth.
4. **Ctrl+C** — already confirmed broken by default, already mitigated, but it
   gets its own line because the failure mode is "the operator cannot stop a
   runaway agent from inside Polis". `tests/keys.rs` asserts `\x03`, and the M7a
   gate includes a manual check: start a long tool call, press Ctrl+C, watch it
   stop.
5. **Idle CPU.** *To measure and record in the ADR:* idle with three panes open
   and silent (target: indistinguishable from today), and CPU during a sustained
   `claude` response. The no-blink cursor, the generation gate and the 30 Hz
   ceiling are the three levers.
6. **The left dock's column floor.** 80 columns is a hard requirement of Claude
   Code's own layout, and on a 1180 pt window with the rail open there may not be
   room for both. *Mitigation:* auto-close the rail before shrinking the dock,
   and say so rather than silently degrading.
7. **CI's Linux leg.** Every platform difference here is behind `cfg!(windows)`,
   but `polis-term` must compile clean on ubuntu from the first commit with
   `-D warnings`, `missing_docs` and clippy `pedantic`.
8. **IME.** Preedit is not rendered in v1, so a CJK or emoji-picker composition
   is invisible until commit — a real regression against a native terminal for
   those operators. Named in the release notes; half a day in M7e.
9. **Dependency surface.** `alacritty_terminal` breaks its API between minors.
   The crate boundary makes replacement a one-file blast radius, and `cargo tree`
   is checked in the first hour so the cost is known rather than discovered.
10. **`run.rs` must not regress.** `polis run -- claude` keeps its inherited
    stdio, foreground child and exit code, unchanged and tested. Only its module
    docs gain a pointer to ADR-0095. **If M7 ever routes `polis run` through a
    PTY, that is a separate decision and a separate ADR.**

---

## Verification

- **Unit / headless, in `polis-term`:** feed recorded byte streams into the
  parser and assert the resulting grid — no window, no PTY. The bulk of the test
  value, and it runs on both CI legs.
- **Key table:** `tests/keys.rs` asserts every row of the table byte-for-byte.
- **Widget:** extend `polis-app/tests/window_frame.rs`, which draws a real frame
  headlessly via `egui::Context::run_ui` with no window or adapter. Note its
  requirement to end each pass with `output.textures_delta.clear()` or epaint
  panics.
- **Idle budget:** assert an attached-but-quiet pane leaves `Repaint::Idle`.
- **Dependency governance:** the existing CI assertion on `cargo tree -p
  polis-hook` must still print exactly one line — `polis-hook` is untouched.
- **End to end, by hand:** `polis work --panes 3` → confirm each session's
  activity lands on the map under the id Polis assigned, Ctrl+C interrupts,
  Shift+Enter writes a second line, the sidebar collapses and restores, and
  closing the window leaves no orphaned `claude.exe`.

---

## M8 — The session server

*Later work. M7 ships first and unchanged in scope.*

tmux, herdr and cmux independently made the same decision: **the PTYs live in a
background process and the UI is a thin client.** tmux made it in 2007 because it
was built for SSH sessions that drop, and the other two inherited the pattern.
M7 as written does the opposite — closing the Polis window, or a GPU driver reset
(not rare on Windows), takes every running agent with it.

### The design — a deliberately dumb daemon

| | Owns |
|---|---|
| **`polis-sessiond`** (new binary, or `polis serve`) | the PTYs; a per-pane raw byte log; the ingest channels (OTLP :4317, hook UDP :45177, fs-notify, transcript tail); an event recording written continuously while detached |
| **the window** (thin client) | the VT parser, the grid, the widget, `World`, the map — **everything from M7, unchanged** |

- **Pane output crosses the boundary as raw bytes, not screens.** This is the one
  decision that keeps M8 cheap. The obvious design — serialise a 45×120 styled
  grid at 30 Hz per pane — is expensive and is why tmux carries so much flow
  control machinery. **tmux control mode does not do that:** `%output %pane
  <data>` ships the raw bytes and the client parses them, which is exactly how
  iTerm2 renders tmux panes as native tabs. So the daemon stays dumb and M7's
  parser, grid and widget move over untouched.
- **Transport:** newline-delimited JSON — `{id, method, params}` / `{id, result}`
  / `{id, error}` — over a named pipe on Windows and a Unix socket elsewhere.
  That is herdr's shape, which is the proven one on this platform pair. Async
  events pushed after an `events.subscribe`, tmux-control-mode style.
- **Reattach** = replay each pane's byte log through the parser to rebuild its
  screen, replay the event recording through the existing `ReplayDriver` to catch
  the map up, then follow live. Both halves already exist. The user-visible
  result is worth stating: you shut the lid for an hour, reopen the window, and
  **watch the detached hour play out on the map** before catching up to live.
- **Flow control:** adopt tmux's `pause-after` → `%pause` / `%continue` with a
  milliseconds-behind figure, replacing M7's naive 30 Hz repaint cap with the
  principled version tmux arrived at after years of it being wrong.
- **IDs carry sigils** — `$session`, `@tab`, `%pane` — and are always preferred
  to names. tmux's own documentation is emphatic about this.

### What it buys

Agents survive the window closing, survive a GPU driver reset, survive a closed
lid. Detach and reattach. And, later, SSH or phone access to a running herd.

### Reused, not built

This is why the estimate is days rather than weeks:

- `Ingest` / `IngestConfig` / `EventSource` move wholesale.
- **ADR-0026's fixed-port bind already makes the ingest a singleton**, and a
  daemon is its natural owner — arguably more natural than the window is today.
- `drain_bus` + `RecordingClock` + `RecordingHeader` already write exactly the
  recording a detached period needs (ADR-0049).
- `ReplayDriver` + `ReplayClock` already replay it, including the idle-gap
  compression that makes a long detached period watchable.
- M7's per-pane byte log becomes pane history.

### Risks

- **Version skew** between daemon and window, which must be detected at attach
  and refused loudly rather than papered over.
- The permanent **"is the daemon running?"** support surface, and stale sockets.
- A **daemon orphaned** with live agents nobody is watching — the failure mode
  that makes people distrust background processes.
- **Named pipes vs Unix sockets** need one abstraction; this is the only genuinely
  platform-forked code in M8.
- **PRD §2 says "no server."** That clause means no cloud, no auth and nothing
  leaving the box — not "no local background process" — but it says the shorter
  thing, so it needs an explicit amendment rather than a quiet reinterpretation.

**Effort: 4–6 dev-days on top of M7**, given the four constraints above. Gets
**ADR-0098**.

### Further out, and deliberately not planned

The city could also move server-side. `polis-render` is already a CPU rasteriser
that draws the city headlessly with no window at all — that is how the GIFs in
`docs/replay/` were produced — so a daemon that renders the map and serves it
remotely is a smaller step than it sounds. It is flagged here and **not** scoped,
because unlike a local daemon it runs squarely into PRD §2's "not a team product,
single operator, local data."

### The rejected third option

**Speak tmux control mode and host real tmux panes**, the way iTerm2 does. It
would give persistence, detach and a battle-tested protocol for free, and it is
genuinely the least-code answer. It is ruled out because **tmux has no native
Windows support**, and Windows is the reference platform. Recorded so it is not
rediscovered and re-costed later.

---

## The ADRs

Four, starting at ADR-0095, in the house Context / Decision / Consequences shape
with measured numbers and named failure modes. The first three land with M7; the
fourth with M8.

**ADR-0095 — Polis grows a terminal, because one inherited console cannot be
several agents.** The reversal, stated honestly: `run.rs`'s three reasons are
re-examined rather than dismissed. Reason 1 ("anything else is a pty emulation
Polis has no reason to write") is satisfied by ConPTY without writing one —
`alacritty_terminal` *is* that code. Reason 2 ("the exit code is the agent's")
does not apply to a window, which has no exit code to donate. Reason 3 ("the
window prints to stderr into the agent's UI") is *fixed* by a pane, not caused by
it. What changed is the requirement: `polis run` watches one agent, and PRD §11's
thesis is several. Records the PTY spike's outcome, the dependency table, the
rejection of `egui_term` and of a hand-written VTE, and M7a's frame-cost
measurements.

**ADR-0096 — The pane's session id is issued, not inferred.** `claude
--session-id <uuid>`. Records why `polis-hook` is not touched (the wire format,
not the env read, is the cost — with the p99 budget and the 8-byte header quoted
from `main.rs`), why the per-pane hook port was rejected despite needing no hook
change, and what the non-`claude` fallback is and why it prefers a false
negative.

**ADR-0097 — Ctrl+C reaches the agent, and `⎿` is drawn, because both were
measured to be broken first.** Two findings that look cosmetic and are not:
`egui-winit 0.36.1 src/lib.rs:1021-1035` never emits Ctrl+C as a key event, fixed
by `raw_input_hook` with the Windows Terminal selection rule; and seven
codepoints Claude Code draws on every screen exist in no font eframe bundles,
fixed by the system's own `seguisym.ttf` at zero binary cost — with the cmap
table showing that bundling Cascadia Mono (363 KiB, SIL OFL, otherwise the
obvious choice) would still miss three of them.

**ADR-0098 (M8) — The agents outlive the window, and the boundary is bytes.**
Records the client–server split and, more importantly, *why it is cheap*: the
daemon ships raw pane bytes rather than rendered screens, following tmux control
mode's `%output` rather than the obvious grid-serialisation design, so the parser
and widget cross the boundary unchanged. Records what was reused rather than
built (ADR-0026's singleton bind, ADR-0049's recording format, `ReplayDriver`),
the rejection of hosting real tmux over control mode (no native Windows), the
version-skew and orphaned-daemon failure modes, and the amendment PRD §2's
"no server" clause needs — that it means no cloud, no auth and nothing leaving
the box, not no local background process.

---

## Files touched

**New:** `polis-term/` (crate + tests), `polis-app/src/panes.rs`, this document.

**Modified:** root `Cargo.toml` (member + pins with justifying comments),
`polis-app/Cargo.toml`, `polis-app/src/lib.rs` (`pub mod panes;` + two rows in
the threading table), `polis-app/src/app.rs` (`Mode::Work`, `Scene.panes`, the
left dock in `draw_scene`, `Repaint::Terminal`, `raw_input_hook`, `on_exit`,
`theme()` installing the terminal family), `polis-app/src/ui.rs`
(`Overlay::terminals`, status chip, help sheet), `polis-app/src/cli.rs` +
`commands.rs` (`polis work`, the `doctor` glyph check),
`polis-app/src/config.rs` (`TerminalConfig`), `polis-app/src/setup.rs` (extract
`cmd_shim`), `polis-app/src/run.rs` (module docs only — **no behaviour change**),
`docs/DECISIONS.md` (ADR-0095/0091/0092), `docs/PRD.md` (§14's crate list gains
`polis-term`; §15 gains M7; §2's non-goals and §1's framing of the terminal as
the problem need amending to admit this feature).

**M8, later:** new `polis-sessiond/` (crate) and a transport module shared with
`polis-app`; `polis-ingest` moves its construction site rather than its code;
`polis-app/src/panes.rs` swaps its PTY host implementation; `docs/DECISIONS.md`
(ADR-0098); `docs/PRD.md` (§2's "no server" clause, §15 gains M8).
