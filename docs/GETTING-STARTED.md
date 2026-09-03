# Getting started with Polis

Polis draws the code you work on as a city seen from above, and plays your
coding agents out on it. Buildings are files, districts are folders, and a
building grows taller as uncommitted work piles up in it — so the skyline points
at whatever most needs looking at.

You do not have to set anything up to see it. Everything below takes a couple of
minutes.

---

## Reading the map

Five things, and then you can read any Polis screen.

| On the map | What it is |
|---|---|
| A **building** | One file. |
| A **district** | One folder. Its buildings are the files in it. |
| **Height** | Uncommitted work. The tallest building is the biggest unreviewed pile. |
| The dense **old core** | The code that was written first. The loose outskirts are the newest. |
| A **cloud** | An agent, hovering over the part of the tree it is working in. It leaves a fading trail behind it. |

Clicking a building selects it and opens that file in your editor. That is the
only thing clicking does. (If it opens the wrong editor, set `POLIS_EDITOR` to a
command template like `code --goto {path}:{line}`.)

The window says all of this itself the first time it opens, and `h` brings it
back at any time — together with what the shapes over a building mean (a hollow
circle is a read, a barred circle an edit, a filled square a write, a filled
triangle a shell command) and what their colour means (teal succeeded, red
failed, grey still running).

---

## Path 1 — see something move, right now

You need one thing you probably already have: Claude Code used at least once on
this machine. If you have ever run `claude`, that is done.

**Windows, without a terminal.** Double-click `Polis.bat` in the Polis folder.
It offers to build Polis the first time, which takes a few minutes and happens
once, then offers to put `polis` on your `PATH`. Then choose
**1. Watch a past session**.

**Anywhere, with a terminal.** In the folder you cloned Polis into
(you need [Rust](https://rustup.rs)):

```
cargo build --release -p polis-app
```

A few minutes the first time — it compiles a graphics stack — and seconds after
that. Then, from that same folder:

```
./target/release/polis
```

On Windows PowerShell that line is `.\target\release\polis.exe`. Both of them
work from a fresh clone with nothing else set up; making the command just
`polis` is [one step further down](#typing-just-polis).

The first time you run it, Polis explains the map and opens a list of every
coding session already recorded on this machine — most recent first (by the last
thing that happened in each one), with the repository, how long it ran and how
much happened in it.

**Pick one** — arrow keys and `Enter`, or click the row, or type in the filter
box to narrow the list first. It replays over that repository's own city: agents
move, files light up as they are edited, the buildings they leave behind grow.

It is a recording, so nothing you do here can break anything.

The window explains itself the first time it opens, and `h` brings that back
along with every key. The ones worth knowing now:

| Key | |
|---|---|
| `Space` | pause and resume |
| `.` `,` | step forward, step back |
| `]` `[` | faster, slower |
| `n` | jump to the next interesting moment |
| `home` `end` | start, end of the recording |
| `t` | swap the map for the filesystem tree, and back |
| `f` | follow the selected thread |
| `i` | the right-hand rail |
| `s` | the streets layer — which files import which |
| `esc` | clear the selection |
| `p` | back to the session list |
| `h` or `?` | what the map is, what the shapes mean, and every key |

Drag to pan, scroll to zoom, arrow keys and `+` `-` do the same. `r` puts the
camera back where it started. Click a building to select it and open that file
in your editor; hover one to see who touched it and when.

Afterwards, `polis watch` opens that list again whenever you want it. Like
`polis map` and `polis replay`, it prints what it is opening and then blocks
until you close the window — that is a window, not a hang.

### Typing just `polis`

Everything above works from `./target/release/polis`. To type `polis` instead,
put that folder on your `PATH`:

```powershell
# Windows PowerShell. Your user PATH, not the machine's; new terminals see it.
[Environment]::SetEnvironmentVariable('Path',
  [Environment]::GetEnvironmentVariable('Path','User') + ';C:\path\to\agentolis\target\release', 'User')
```

```sh
# macOS / Linux. Put the same line in ~/.bashrc or ~/.zshrc to keep it.
export PATH="$PWD/target/release:$PATH"
```

Two shortcuts: `Polis.bat` offers to do it for you on Windows, and `polis
doctor` prints whichever of these lines applies with this machine's own path
already filled in. Every `polis …` command in the rest of this guide assumes you
have done it; if you have not, spell out `./target/release/polis` instead.

---

## Path 2 — connect your own agents

Two ways, and the first needs no configuration at all.

### The easy one: let Polis start the agent

```
polis run -- claude
```

This starts Claude Code for you, in this terminal, exactly as if you had typed
`claude` — same prompt, same keys, same exit code. The difference is that Polis
has already set up everything the agent needs to report what it is doing, opened
the map beside it, and started listening. Nothing is changed in your shell or in
any configuration file.

One honest caveat while you are here: today that map window shows the city, and
the session plays back **afterwards** rather than live. Polis is listening and
counting the whole time — it tells you how much it heard when the agent exits —
and drawing it as it happens is the next thing being built.

Anything you would normally pass to `claude` goes after the `--`:

```
polis run -- claude --resume
polis run -- claude -p "fix the failing test"
```

When the agent exits, Polis prints what it saw and the one command that replays
the session you just ran.

### The other one: let Polis watch agents you start yourself

If you would rather keep typing `claude` on its own, Polis needs a small block
added to Claude Code's settings file so it gets told what is happening.

```
polis connect
```

It shows you the exact file it wants to write, the exact lines that change, and
takes a backup — then asks. Nothing is written until you say yes.

**Which settings file.** By default that is *this repository's*
`.claude/settings.json`, so it covers agents you start in this checkout and no
others. For every repository on this machine, use the user one instead:

```
polis connect --user
```

Connecting in one repository and then starting `claude` in another is the
commonest reason for "I connected and nothing happened". The consent screen
names which of the two it is about to write, every time.

To undo either:

```
polis connect --uninstall
```

which removes what it added and puts the file back.

One thing that looks like a failure and is not: in an interactive session Claude
Code runs no settings-file hooks until you have accepted the workspace trust
dialog for that folder. Accept it once, and they start firing.

---

## When something is wrong

```
polis doctor
```

Every line that is not `ok` comes with the exact command that fixes it. Some of
them Polis can do itself:

```
polis doctor --fix
```

---

## Building Polis

You need [Rust](https://rustup.rs). Then, in the Polis folder:

```
cargo build --release -p polis-app
```

The first build compiles a graphics stack and takes a few minutes. After that it
is seconds. The binary lands in `target/release/polis`
(`target\release\polis.exe` on Windows) and runs from there — see
[typing just `polis`](#typing-just-polis) for the one line that shortens it.

There is a second, optional build:

```
cargo build --profile hook -p polis-hook
```

`polis-hook` is the tiny program Claude Code runs to tell Polis what is
happening. `polis run` does not need it; `polis connect` does, and `polis doctor
--fix` will build it for you when it is missing.

On Windows you can skip all of this and double-click `Polis.bat`, which offers to
build it for you.

---

## Every command

| | |
|---|---|
| `polis` | The first run explains itself, then opens the session picker. Afterwards, maps the folder you are in. |
| `polis watch` | Pick a past session and watch it replay. Opens a window and blocks until you close it. |
| `polis map` | This repository as a city, right now. A window, and blocks. |
| `polis run -- claude` | Start an agent with the map watching. |
| `polis connect` | Let Polis see agents you start yourself, in this repository. `--user` for every repository; `--uninstall` reverses either. |
| `polis doctor` | What is wrong, and how to fix it. `--fix` applies what it can. |
| `polis snapshot --out city.png` | Save a picture of the city instead of opening a window. |
| `polis replay <file>` | Replay one specific recording. A window, and blocks. |
| `polis tail` | The raw event stream as text, no graphics. |
| `polis env` | Print the settings an agent needs, to paste somewhere yourself. |

Add `--repo <path>` to any of them to point at a different folder, and `--help`
to any of them for the details.

Polis needs a git repository with at least one commit: the city is built out of
git history, so a folder that is not a checkout has nothing to draw and a
checkout with no commits yet draws an empty one. Both say so on screen, with the
command that fixes them.

---

## Things worth knowing

**Nothing leaves your machine.** Polis reads files on this computer and listens
on two loopback ports. There is no server, no account and no network traffic
leaving the box. It never asks agents to export your prompts or their replies.

**One repository, one city.** Polis maps a single checkout, worktrees included.

**The map is stable.** The same repository always produces the same city, on
every launch and on every machine, because remembering where things are is the
entire point. A file added today appears on the outskirts; it does not shuffle
everything you had learned.
