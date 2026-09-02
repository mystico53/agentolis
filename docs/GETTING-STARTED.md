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

Click a building and the file opens in your editor. That is the only thing
clicking does. (If it opens the wrong editor, set `POLIS_EDITOR` to a command
template like `code --goto {path}:{line}`.)

---

## Path 1 — see something move, right now

You need two things. Claude Code used at least once on this machine — if you have
ever run `claude`, that is done. And Polis itself built: on Windows the first
option below does that for you, and everywhere else it is
[two commands](#building-polis).

**Windows, without a terminal.** Double-click `Polis.bat` in the Polis folder.
It will offer to build Polis the first time, which takes a few minutes and
happens once. Then choose **1. Watch a past session**.

**Anywhere, with a terminal.**

```
polis
```

The first time you run it, Polis explains the map and opens a list of every
coding session already recorded on this machine — most recent first, with the
repository, how long it ran and how much happened in it. Pick one. It replays
over that repository's own city: agents move, files light up as they are edited,
the buildings they leave behind grow.

It is a recording, so nothing you do here can break anything.

| Key | |
|---|---|
| `Space` | pause and resume |
| `.` `,` | step forward, step back |
| `]` `[` | faster, slower |
| `n` | jump to the next interesting moment |
| `p` | back to the session list |
| `h` or `?` | every key, on screen |

Drag to pan, scroll to zoom, arrow keys and `+` `-` do the same. `r` puts the
camera back where it started.

Afterwards, `polis watch` opens that list again whenever you want it.

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
takes a backup — then asks. Nothing is written until you say yes. To undo it:

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
cargo build --profile hook -p polis-hook
```

The first build compiles a graphics stack and takes a few minutes. After that it
is seconds. The binary lands in `target/release/polis`
(`target\release\polis.exe` on Windows); put that folder on your `PATH` and
`polis` works from anywhere.

On Windows you can skip all of this and double-click `Polis.bat`, which offers to
build it for you.

The second line builds `polis-hook`, the tiny program Claude Code runs to tell
Polis what is happening. `polis run` does not need it; `polis connect` does.

---

## Every command

| | |
|---|---|
| `polis` | The first run explains itself, then opens the session picker. Afterwards, maps the folder you are in. |
| `polis watch` | Pick a past session and watch it replay. |
| `polis map` | This repository as a city, right now. |
| `polis run -- claude` | Start an agent with the map watching. |
| `polis connect` | Let Polis see agents you start yourself. `--uninstall` reverses it. |
| `polis doctor` | What is wrong, and how to fix it. `--fix` applies what it can. |
| `polis snapshot --out city.png` | Save a picture of the city instead of opening a window. |
| `polis replay <file>` | Replay one specific recording. |
| `polis tail` | The raw event stream as text, no graphics. |
| `polis env` | Print the settings an agent needs, to paste somewhere yourself. |

Add `--repo <path>` to any of them to point at a different folder, and `--help`
to any of them for the details.

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
