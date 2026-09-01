# `polis-hook` transport — design, measurements, and safety proof

**Scope:** PRD §4.2 (Channel B), §13.1 (hook budget), §16 (hook safety).
**Status:** settled. Every number below was produced by running code on this machine today.

**Measurement environment.** Windows 11 Pro 10.0.26200, `x86_64-pc-windows-msvc`, 24 logical cores.
`rustc 1.94.0 (4a4ef493e 2026-03-02)`, LLVM 21.1.8, `cargo 1.94.0`. Node `v24.14.0`.
Probe workspace: `C:\Users\konka\AppData\Local\Temp\claude\C--coding-agentolis\6f51089f-1ec0-4e78-9bc9-8ff6864e0100\scratchpad\hook-probe`.

Claims about Linux and macOS are marked **UNVERIFIED** and must be re-measured on those CI legs.
Nothing in this document about a non-Windows platform was executed.

---

## 0. Decisions, in one place

| # | Decision | Why, in one line |
|---|---|---|
| 1 | **Loopback UDP on every platform.** No `AF_UNIX` anywhere. | `AF_UNIX`+`SOCK_DGRAM` cannot be created on Windows at all, so Windows is UDP regardless; a second transport under `#[cfg(unix)]` doubles the safety matrix to buy properties UDP already has. |
| 2 | **Unconnected `send_to`, fresh socket per process, result discarded.** | Structurally immune to `WSAECONNRESET`: there is no connection to latch the error onto, and the socket is destroyed before ICMP feedback could arrive. |
| 3 | **Send cap 61 440 bytes (60 KiB), `TRUNCATED` bit in the header.** | A UDP datagram tops out at 65 507 payload bytes; 65 508 fails with `WSAEMSGSIZE` and the event vanishes. The PRD's 256 KiB cap is unsendable. |
| 4 | **8-byte header: `u32` tag (bit 31 = truncated) + `u32` length, both little-endian.** | Matches PRD §4.2 step 2 exactly; LE means no byte swap on either end. |
| 5 | **Endpoint: env var → endpoint file → compiled-in `127.0.0.1:45177`.** | The file costs nothing measurable (p50 17 µs against a 4.7 ms spawn floor), so zero-config discovery is free. |
| 6 | **Daemon binds the fixed port `127.0.0.1:45177`, never an ephemeral one.** | 45177 sits below the OS ephemeral range (measured 49152+), so it can never be transiently stolen; and the bind doubles as the singleton lock. |
| 7 | **Register the hook in exec form (`"args": [...]`), never shell form.** | Measured: shell form costs 6× (Git Bash) to 24× (PowerShell) the exec form, per event. |
| 8 | **Install a panic hook that calls `exit(0)`; use `args_os()`, never `args()`.** | Measured: without them, a non-Unicode argv element aborts the process with `0xC0000409`, which cancels the agent's tool call. |
| 9 | **500 ms watchdog thread.** | Bounds a stuck `read(stdin)` at half a second instead of Claude Code's 5 s hook timeout. Measured cost: below the noise floor. |
| 10 | **Split the 3 ms budget** into in-process (≤ 3 ms, met: p99 1.23 ms) and end-to-end spawn (an OS+runtime constant Polis does not control). | The end-to-end number is 80 % the parent runtime's `child_process` cost, measured with a binary that only calls `exit(0)`. |

---

## 1. Transport: loopback UDP everywhere

### 1.1 The PRD's transport is not implementable

PRD §4.2 step 3 specifies *"one non-blocking `sendto()` on a Unix **datagram** socket at `$XDG_RUNTIME_DIR/polis.sock`."*
Re-verified from scratch here by calling `socket()` directly through `ws2_32`:

```
AF_UNIX  + SOCK_STREAM  -> created OK
AF_UNIX  + SOCK_DGRAM   -> FAILED, WSA error 10047 (WSAEAFNOSUPPORT)
AF_INET  + SOCK_DGRAM   -> created OK

XDG_RUNTIME_DIR = Err(NotPresent)
LOCALAPPDATA    = Ok("C:\\Users\\konka\\AppData\\Local")
```

Windows `AF_UNIX` is **stream-only**. Both halves of step 3 — the address family and the path — fail on
Windows. This independently confirms the hooks agent's finding.

### 1.2 Why not UDP on Windows + `AF_UNIX` dgram under `#[cfg(unix)]`

Windows must use UDP. The live question is whether Unix should get a *second* transport. It should not.

**What a dual transport costs.** Two send paths, two error taxonomies, two receiver implementations in
`polis-ingest`, two stale-endpoint stories (a stale socket *inode* behaves nothing like a stale *port*),
and — decisively — **two safety matrices**. §16's hook-safety assertions are the load-bearing test in this
component; every row in §8 below would have to be re-proven against a different set of `errno`s. The PRD's
own §14 says `polis-hook` must have "a near-empty dependency tree"; the same logic applies to its
behavioural surface.

**What a dual transport would buy, and why UDP already has it:**

| `AF_UNIX` `SOCK_DGRAM` property | How loopback UDP gets it here |
|---|---|
| Filesystem ACLs restrict who can send | The endpoint file lives in `%LOCALAPPDATA%` / `$XDG_RUNTIME_DIR`, both per-user. See §1.4 on what this does and does not buy. |
| Cannot escape the machine | The hook binds its **source** to `127.0.0.1`. Measured: such a socket returns `NetworkUnreachable` (WSA 10051) for any non-loopback destination. Combined with `parse_endpoint` rejecting non-loopback literals, a tampered endpoint file cannot exfiltrate a payload. |
| No port-space collision | Fixed port 45177, measured outside the OS ephemeral range (§4.3). |
| No stale filesystem node to unlink | UDP has no node at all. A stale *endpoint file* is a text file the next daemon overwrites; a stale *socket file* needs unlink-on-start logic and a liveness probe. UDP is strictly simpler here. |

**What a dual transport would additionally cost on Unix**, and the reason to be glad it is out of scope:
macOS caps a Unix datagram at `net.local.dgram.maxdgram`, whose default is well below 60 KiB — a 60 KiB
frame that works on Linux would fail on macOS. That is a per-platform tuning trap the UDP path does not
have, since the 65 507-byte limit is an IPv4 constant. **UNVERIFIED** (Windows-only box); flagged for the
macOS CI leg only as a reason not to go down this road, not as a claim about a design we are not shipping.

**Decision: one transport, loopback UDP, everywhere.** One wire format, one receiver, one safety matrix,
one set of numbers in this document.

### 1.3 What loopback UDP actually guarantees, measured

| Property the PRD requires | Measured |
|---|---|
| Never blocks | 20 000 × 1 KiB sends from one non-blocking socket at a non-draining receiver: `WouldBlock=0`, `other_err=0`, 65.55 ms total. Windows loopback UDP send never parks and never even returns `EAGAIN`. |
| Absent daemon is not an error | `send_to` to a closed loopback port: `Ok(n)`. 1000 consecutive fresh-socket sends to a closed port: **0 errors**. |
| No filesystem node needed | Connectionless. Nothing to create, nothing to unlink, nothing to leave stale. |
| Never a FIFO / named pipe | UDP has no open-side rendezvous. Nothing can block pending a server instance. |

`SO_SNDBUF` default 65 536; `SO_RCVBUF` default 65 536.

### 1.4 The one property UDP does not have, stated honestly

Any local process that knows the port can send a datagram to the daemon. The endpoint file being in a
per-user directory makes the port *inconvenient* to learn, not *secret* — the compiled-in default is public.
So: **a local process can inject forged events into the Polis daemon.**

The blast radius is a visualization drawing something untrue. It cannot execute anything, and the daemon
must already parse every payload defensively (PRD §4.4). The daemon's frame validation in §9 rejects
essentially all accidental noise. If a future threat model needs sender authentication, the place to add it
is a shared secret in the endpoint file mixed into a longer header — which would change the wire format, so
it is called out here rather than left implicit.

The *outbound* direction, which matters more because hook payloads contain source code, **is** closed:
loopback-only source bind plus loopback-only destination parsing, both measured.

---

## 2. The `WSAECONNRESET` gotcha, and how it is neutralized

**The hazard.** On Windows, a UDP datagram sent to a closed port can provoke an ICMP port-unreachable, and
the stack surfaces that to the *sender's socket* as `WSAECONNRESET` (10054) on a **later** call. A hook that
sends to a dead daemon could therefore fail on its next send. This is exactly the failure the daemon's
absence must not cause.

### 2.1 Measured on this machine

Target: an ephemeral port that was bound and then dropped, so it is definitely closed. Sends 200 ms apart,
well past any ICMP round trip on loopback.

```
[A] UNCONNECTED send_to, SAME socket, 2 sends, no listener
    send #0: Ok(5)
    send #1: Ok(5)

[B] CONNECTED send(), SAME socket, 4 sends, no listener   <- the classic gotcha
    send #0: Ok(5)   send #1: Ok(5)   send #2: Ok(5)   send #3: Ok(5)

[C] CONNECTED, same socket, SIO_UDP_CONNRESET disabled first
    SIO_UDP_CONNRESET=off applied: true
    send #0: Ok(5)   send #1: Ok(5)   send #2: Ok(5)   send #3: Ok(5)

[D] the real polis-hook binary, 4 runs in a row, no listener, 200 ms apart
    run #0: exit=0   run #1: exit=0   run #2: exit=0   run #3: exit=0

[E] 1000 fresh-socket unconnected sends to the closed port
    errors: 0/1000   kinds: []

[F] daemon-side: a receiver socket that never sends, 50 datagrams
    no SIO_UDP_CONNRESET fix: got 50 datagrams, 0 recv errors
```

A separate probe against a **non-loopback** local address (`192.168.1.123 → 192.168.1.123:9`, connected,
4 sends) also returned `Ok` every time.

**Honest reading: the gotcha did not reproduce on this machine, in any configuration, including the
connected one that is supposed to trigger it.** Most likely the local firewall is suppressing the inbound
ICMP that drives the feedback. That is a property of *this box*, not of Windows, and it is exactly the kind
of thing that differs on a customer machine with a different firewall or a third-party Winsock LSP. So the
design must not depend on it being absent.

### 2.2 The neutralization is structural, not configuration

Three independent reasons the hook cannot be hurt by this, none of which depend on the measurement above:

1. **The socket is never `connect()`-ed.** `send_to` on an unconnected socket has no connection state for
   the stack to attach a delayed error to. This is why `SIO_UDP_CONNRESET` is not needed in the hook at all
   — the ioctl exists to suppress an error that an unconnected socket cannot receive.
2. **One socket, one send, then process exit.** ICMP feedback for send *N* can only be reported on send
   *N+1* on the same socket. There is no send *N+1*: the socket is created, used once, and destroyed with
   the process. The failure mode requires a socket lifetime the hook does not have.
3. **The send result is discarded.** `let _ = sock.send_to(...)`. Even if `WSAECONNRESET` were returned, it
   changes nothing: the event is dropped and the process exits 0. Row 1 of the safety matrix (§8) asserts
   this over 50 consecutive runs against a closed port, and `[D]` above repeats it with the real binary
   twice in a row, as required.

**Daemon side:** `[F]` shows a receive-only socket saw no resets. Apply `SIO_UDP_CONNRESET = 0` anyway
(measured: the `WSAIoctl` returns success) — it is one call at startup and it removes an entire class of
`recv_from` error that would otherwise need handling in the ingest loop.

---

## 3. Wire format

One datagram per hook invocation. Header is 8 bytes, exactly as PRD §4.2 step 2 requires.

```
 byte   0 1 2 3   4 5 6 7   8 ...........................
       +---------+---------+-----------------------------+
       |   tag   |   len   |  payload (raw hook stdin)   |
       +---------+---------+-----------------------------+
         u32 LE    u32 LE          len bytes

 tag  bits  0..=30   event kind, 0 = Unknown  (table in §12)
      bit      31    TRUNCATED — stdin was larger than the cap
 len            number of payload bytes in THIS datagram
                (NOT the original stdin size, which is unrecoverable)
```

**Endianness: little-endian, fixed by decision, not by convention.** This is a same-machine loopback
protocol between two processes on the same CPU; every target Polis supports (`x86_64`, `aarch64`) is
little-endian. LE lets both ends use `to_le_bytes` / `from_le_bytes` with no byte swap. Do not "fix" this to
network order later — it would be a silent wire break with no benefit.

**Where the truncation bit lives, precisely:** `tag & 0x8000_0000`. The event kind is `tag & 0x7FFF_FFFF`.
The hook masks the bit off the argv-derived kind before OR-ing it back in, so no event name can forge it
(unit-tested).

Verified against the real binary, receiving on a real socket:

| stdin | datagram | tag | kind | trunc | len | `len + 8 == datagram` |
|---|---|---|---|---|---|---|
| 0 B | 8 B | `0x00000009` | 9 | 0 | 0 | yes |
| 1 024 B | 1 032 B | `0x00000009` | 9 | 0 | 1 024 | yes |
| 61 440 B (cap) | 61 448 B | `0x00000009` | 9 | 0 | 61 440 | yes |
| 61 441 B (cap+1) | 61 448 B | `0x80000005` | 5 | **1** | 61 440 | yes |
| 102 400 B | 61 448 B | `0x80000009` | 9 | **1** | 61 440 | yes |
| 262 144 B (PRD cap) | 61 448 B | `0x80000009` | 9 | **1** | 61 440 | yes |
| 8 MiB | 61 448 B | `0x80000005` | 5 | **1** | 61 440 | yes |
| 32 MiB | 61 448 B | `0x80000005` | 5 | **1** | 61 440 | yes |

The boundary is exact: 61 440 is clean, 61 441 sets the bit.

---

## 4. The datagram-size problem the PRD missed

### 4.1 The bug

PRD §4.2 step 1 caps stdin at 256 KiB and ships it. A UDP datagram cannot carry 256 KiB. Measured sweep,
receiver at `SO_RCVBUF` = 8 MiB:

```
size    1024: send Ok(1024)                            recv 1024
size    8192: send Ok(8192)                            recv 8192
size   32768: send Ok(32768)                           recv 32768
size   61448: send Ok(61448)                           recv 61448
size   65507: send Ok(65507)                           recv 65507      <- exact limit
size   65508: send Err(raw=10040 WSAEMSGSIZE)          recv TimedOut
size   65536: send Err(raw=10040 WSAEMSGSIZE)          recv TimedOut
size  131072: send Err(raw=10040 WSAEMSGSIZE)          recv TimedOut
size  262152: send Err(raw=10040 WSAEMSGSIZE)          recv TimedOut   <- the PRD's 256 KiB + header
```

As written, **every event over ~64 KiB would vanish silently** — `send_to` fails, the hook discards the
error (correctly, per the never-fail contract) and exits 0. Large `PermissionRequest` and `PreToolUse`
payloads carrying a big `tool_input.new_string` are exactly the events most worth seeing.

A common belief is that `SO_SNDBUF` bounds the maximum datagram. Measured false on Windows — a 61 448-byte
datagram was delivered with `SO_SNDBUF` set to 0, 1024, 8192, 32768, and 65536:

```
SO_SNDBUF set=0      effective=0       send Ok(61448)   recv 61448
SO_SNDBUF set=1024   effective=1024    send Ok(61448)   recv 61448
SO_SNDBUF set=8192   effective=8192    send Ok(61448)   recv 61448
```

### 4.2 The fix

`MAX_PAYLOAD = 60 * 1024 = 61 440`, so the framed datagram is 61 448 bytes — 4 059 bytes under the hard
limit. The slack is deliberate: it absorbs any future header growth without a wire break, and stays clear of
the exact boundary where a stack quirk would be most likely to bite.

The hook reads `MAX_PAYLOAD + 1` bytes so overflow is detectable without a second syscall, truncates, sets
bit 31, sends, and only *then* drains the rest of stdin. **Send before drain** is the important ordering: a
32 MiB payload delays nothing that matters, and the watchdog bounds the drain.

The daemon treats a truncated event as notification-only and backfills detail from the JSONL transcript
(PRD §4.4). It cannot recover the original size — `len` describes this datagram, not the original — which
is the right trade for keeping the header at 8 bytes.

### 4.3 Why the drain exists

Exiting with bytes still in the pipe makes the *agent's* write fail. Proven not to happen, from a Node
parent (the runtime Claude Code runs on), watching for an `error` event on the child's stdin stream:

| stdin bytes | child exit | signal | **parent stdin error** |
|---|---|---|---|
| 0 | 0 | null | none |
| 1 024 | 0 | null | none |
| 61 440 | 0 | null | none |
| 61 441 | 0 | null | none |
| 102 400 | 0 | null | none |
| 262 144 | 0 | null | none |
| 1 048 576 | 0 | null | none |
| 8 388 608 | 0 | null | none |
| 33 554 432 | 0 | null | none |

No `EPIPE`, no `ERROR_NO_DATA`, at any size up to 32 MiB.

---

## 5. Endpoint discovery, zero config

### 5.1 Resolution order

1. `POLIS_HOOK_ENDPOINT` env var — explicit override. Free (`env::var_os` reads the already-mapped
   process environment block; measured p50 **0.000 ms**, max 0.023 ms over 20 000 iterations).
   Note this cannot be the primary mechanism: it only reaches agents the daemon launched itself, and
   operators start `claude` from their own terminal. It is for tests and for daemon-spawned agents.
2. **Endpoint file** — the zero-config path.
   - Windows: `%LOCALAPPDATA%\polis\endpoint`
   - Unix: `$XDG_RUNTIME_DIR/polis/endpoint`, falling back to `$XDG_STATE_HOME`, then
     `~/.local/state/polis/endpoint`. The fallbacks are not optional: `XDG_RUNTIME_DIR` is measurably unset
     in real environments (it is unset in this one), and a hook that only looks there finds nothing.
   - Read is bounded to 128 bytes. First line only; the daemon may append diagnostics below it.
     Decoded with `from_utf8_lossy` so a corrupt file fails to *parse* rather than failing to be *read*.
3. Compiled-in default `127.0.0.1:45177`.

Only a **literal IPv4 loopback** `a.b.c.d:port` with a non-zero port is accepted. `ToSocketAddrs` is
deliberately not used, because a hostname would put a DNS lookup on the agent's critical path:

```
to_socket_addrs("127.0.0.1:45177")        n=5000  p50=0.000  p99=0.000  max=0.002 ms
to_socket_addrs("localhost:45177")        n=200   p50=0.127  p99=0.522  max=4.193 ms   <- DNS
to_socket_addrs("nonexistent.invalid:…")  n=20    p50=0.141  p99=2.066  max=2.066 ms   <- failing DNS
```

A single failing name resolution can consume **4.2 ms — more than the entire in-process budget** — for a
value that is always `127.0.0.0/8`. Literal parsing also confines a tampered endpoint file (§1.2).

### 5.2 Does the file read fit the budget? Yes, with three orders of magnitude to spare

Isolated cost:

| Operation | n | p50 | p99 | max |
|---|---|---|---|---|
| `read_to_string` on the existing endpoint file | 20 000 | **0.017 ms** | 0.040 ms | 3.616 ms |
| `read_to_string` on a missing file (ENOENT) | 20 000 | **0.004 ms** | 0.006 ms | 0.085 ms |
| `env::var(POLIS_HOOK_ENDPOINT)` | 20 000 | 0.000 ms | 0.000 ms | 0.023 ms |

End-to-end, interleaved round-robin so background load hits all three equally, n = 300 each after 20 warmups:

| Resolution path | min | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| env var set (no file lookup at all) | 4.696 | **5.439** | 6.295 | 7.571 | 8.409 |
| endpoint file read (zero-config path) | 4.781 | **5.504** | 6.243 | 7.523 | 8.715 |
| no env var, no file (ENOENT → default) | 4.785 | **5.481** | 6.302 | 7.865 | 9.084 |

The three are statistically indistinguishable — 65 µs of p50 spread against a 4.7 ms spawn floor.
**The endpoint file is free. Keep it as the primary mechanism; the fixed default port is the fallback, not
the other way round.**

Correctness of the path, both directions verified against the real binary:

```
file-only routing: exit=0, datagrams at the file's port = 1   (must be 1)
precedence:        env-var port got 1, endpoint-file port got 0   (must be 1 and 0)
```

### 5.3 The daemon binds a fixed port, never an ephemeral one

Measured, with 64 ephemeral binds sampled for the range:

```
[1] bind 127.0.0.1:45177 twice
    first  bind: OK
    second bind: refused AddrInUse raw=10048       <- a usable singleton lock

[2] squatter on 0.0.0.0:45177, then polis binds 127.0.0.1:45177
    squatter bind 0.0.0.0: OK
    polis  bind 127.0.0.1: SUCCEEDED despite the squatter
    -> 127.0.0.1-bound socket got Ok(11); 0.0.0.0-bound socket got TimedOut
       (the more specific bind wins: polis still receives its own traffic)

[3] 64 ephemeral binds landed in 57132..=57195
    45177 is OUTSIDE the observed ephemeral range
```

`netsh int ipv4 show dynamicport udp` reports the dynamic range as **49152 + 16384**, i.e. 49152–65535.
45177 is below it.

This matters more than it looks. Had the daemon used an ephemeral port recorded in the endpoint file, a
crash could leave the file pointing at a port the OS later reassigns to **an unrelated local process** — and
hook payloads contain source code. A fixed port outside the dynamic range makes that impossible.

### 5.4 Stale file and two daemons — the policy

**Stale endpoint file (daemon crashed, file survives).** Nothing needs to happen. The hook sends to a closed
port; measured, that is `Ok(n)`, exit 0, and *cheaper* than the live case (1 KiB: p50 4.610 ms with no
listener vs 4.735 ms with one). No hook ever waits on a dead daemon, no cleanup is required for
correctness, and no timeout is involved. The next daemon start overwrites the file.

Write it **atomically** — temp file in the same directory, then rename — so a hook can never read a
half-written line. Delete it best-effort on clean shutdown: that makes hooks fall back to 45177, which is
exactly where the next daemon will bind.

Recommended contents (first line is load-bearing, the rest is for humans and `polis doctor`):

```
127.0.0.1:45177
pid=4242
boot=2026-09-01T10:00:00Z
```

**Two daemons.** The `bind(127.0.0.1:45177)` in `[1]` above *is* the lock — no lockfile, no PID liveness
check, no race. The second daemon gets `AddrInUse` and must exit with a clear message
("another polis is already receiving on 127.0.0.1:45177 — pid from `%LOCALAPPDATA%\polis\endpoint`"),
not silently bind elsewhere. Silently taking another port is the failure that produces a half-populated map
and an operator who cannot tell which window is lying.

**Port squatted by unrelated software.** `[2]` shows a wildcard (`0.0.0.0`) squatter does not block Polis
and does not steal its traffic. Only another `127.0.0.1:45177` bind conflicts. If that happens, Polis exits
and the operator passes `--port N`, which is recorded in the endpoint file — the one case where the file
carries a value the compiled default does not.

---

## 6. Latency

### 6.1 Methodology, and why the first attempt was wrong

Three things had to be got right before the numbers meant anything.

1. **Interleave, do not block.** The first stage-decomposition run measured each stage as a block of 400
   spawns. It produced a p99 of 114 ms for a binary whose entire body is `exit(0)`, and *negative* marginal
   costs — background load (Defender, other agents) had landed on whichever stage was unlucky. Re-run
   round-robin, every stage eats the same noise and the differences become real. Every table below that
   compares variants is interleaved.
2. **Measure two different things and never average them.** In-process wall time is measured *inside* the
   hook, from the first statement of `main` to the return of `send_to`, and printed *after* the measurement
   window closes so the print cannot contaminate it. End-to-end is measured in the parent, from before
   `spawn` to after `wait`.
3. **Measure end-to-end from the parent that actually pays it.** Claude Code is Node. A Rust parent's
   `std::process::Command` is a thin `CreateProcess` wrapper; Node's `child_process.spawn` is not. Both are
   reported below, because the difference between them *is* the finding.

Payloads are shaped like a real hook stdin blob (a JSON object with `session_id`, `hook_event_name`,
`tool_name`, `tool_input`), padded to size. 20–25 warmup spawns are discarded before each run.

### 6.2 In-process wall time — stdin read → framed → sent → exit

`n = 400` per row, cumulative from `main` entry. All times in **ms**.

| Payload | Listener | Phase | min | p50 | p90 | **p99** | max |
|---|---|---|---|---|---|---|---|
| 1 KiB | yes | stdin read + frame | 0.049 | 0.071 | 0.083 | 0.109 | 0.143 |
| 1 KiB | yes | + resolve endpoint | 0.050 | 0.073 | 0.086 | 0.111 | 0.145 |
| 1 KiB | yes | + bind + set_nonblocking | 0.678 | 0.812 | 0.897 | 1.094 | 1.432 |
| **1 KiB** | **yes** | **+ send_to = TOTAL** | 0.748 | **0.894** | 0.992 | **1.234** | 1.561 |
| 1 KiB | no | stdin read + frame | 0.049 | 0.071 | 0.082 | 0.101 | 0.169 |
| 1 KiB | no | + resolve endpoint | 0.050 | 0.073 | 0.084 | 0.104 | 0.171 |
| 1 KiB | no | + bind + set_nonblocking | 0.672 | 0.775 | 0.860 | 0.940 | 1.200 |
| **1 KiB** | **no** | **+ send_to = TOTAL** | 0.721 | **0.833** | 0.924 | **1.006** | 1.254 |
| 100 KiB | yes | stdin read + frame | 0.084 | 0.111 | 0.128 | 0.147 | 0.241 |
| 100 KiB | yes | + resolve endpoint | 0.085 | 0.113 | 0.130 | 0.151 | 0.242 |
| 100 KiB | yes | + bind + set_nonblocking | 0.714 | 0.822 | 0.911 | 1.060 | 1.202 |
| **100 KiB** | **yes** | **+ send_to = TOTAL** | 0.792 | **0.911** | 1.013 | **1.151** | 1.339 |
| 100 KiB | no | stdin read + frame | 0.078 | 0.113 | 0.130 | 0.154 | 0.179 |
| 100 KiB | no | + resolve endpoint | 0.079 | 0.115 | 0.131 | 0.158 | 0.181 |
| 100 KiB | no | + bind + set_nonblocking | 0.715 | 0.839 | 0.919 | 1.090 | 1.231 |
| **100 KiB** | **no** | **+ send_to = TOTAL** | 0.771 | **0.905** | 0.993 | **1.163** | 1.303 |

**Worst in-process p99 across the whole matrix: 1.234 ms. Worst single observation: 1.561 ms.**
The 3 ms in-process budget holds with a 2.4× margin.

Three things fall out of the phase breakdown:

- **`UdpSocket::bind` dominates**, at 0.74 ms of the 0.89 ms p50 — that is Winsock initialising
  (`WSAStartup` + `ws2_32`/`mswsock` load) on first socket use, not the bind itself. It is the single
  largest in-process cost and it is not reducible from Rust.
- **Payload size is nearly free.** 100 KiB costs 40 µs more than 1 KiB at p50, because the hook stops
  reading at 60 KiB + 1 and drains the rest after the send.
- **The listener's presence does not matter.** No-listener is marginally *faster*, consistent with
  connectionless send.

### 6.3 End-to-end spawn → exit, Rust parent (`std::process::Command`)

`n = 400` per row after 20 warmups. This is the cost with the thinnest possible `CreateProcess` wrapper.

| Binary | Payload | Listener | min | p50 | p90 | **p99** | max | delivered | non-zero exits |
|---|---|---|---|---|---|---|---|---|---|
| `noophook` (`exit(0)` only) | 1 KiB | yes | 3.079 | **3.300** | 3.638 | **3.997** | 4.196 | — | 0 |
| `polis-hook` | 1 KiB | yes | 4.292 | **4.735** | 5.143 | **5.483** | 6.002 | 420/420 | 0 |
| `polis-hook` | 1 KiB | no | 4.228 | **4.610** | 4.898 | **5.395** | 5.829 | n/a | 0 |
| `polis-hook` | 100 KiB | yes | 4.371 | **4.780** | 5.188 | **6.179** | 6.578 | 420/420 | 0 |
| `polis-hook` | 100 KiB | no | 4.321 | **4.686** | 5.048 | **5.540** | 6.336 | n/a | 0 |

At 100 KiB the receiver logged 25 808 160 bytes over 420 datagrams = **61 448 bytes each**: truncation
confirmed on the wire under load, not just in the isolated wire-format check.

### 6.4 End-to-end spawn → exit, **Node parent** (`child_process.spawn`) — the number the agent pays

`n = 400` per row after 25 warmups, piped stdio, `windowsHide`.

| Binary | Payload | Listener | min | p50 | p90 | **p99** | max | delivered | non-zero exits |
|---|---|---|---|---|---|---|---|---|---|
| `noophook` (`exit(0)` only) | 1 KiB | yes | 9.786 | **10.635** | 12.075 | **13.810** | 15.620 | — | 0 |
| `polis-hook` | 1 KiB | yes | 12.007 | **14.547** | 17.015 | **22.147** | 85.902 | 425/425 | 0 |
| `polis-hook` | 1 KiB | no | 11.882 | **13.123** | 15.533 | **19.273** | 25.253 | n/a | 0 |
| `polis-hook` | 100 KiB | yes | 12.063 | **13.095** | 13.931 | **15.643** | 19.887 | 425/425 | 0 |
| `polis-hook` | 100 KiB | no | 11.712 | **12.859** | 13.639 | **15.362** | 21.822 | n/a | 0 |

**This is the key result of the whole exercise.** A binary whose entire body is `exit(0)` costs
**10.6 ms p50 / 13.8 ms p99** when spawned from Node. `polis-hook` costs 13–14.5 ms p50. So roughly
**80 % of what the agent pays is Node's own `child_process` machinery**, and Polis's marginal contribution
is ~2.5–3.9 ms of end-to-end, of which ~0.9 ms is the in-process work in §6.2 and the rest is the larger
image plus Winsock DLL load in the child.

This reproduces and explains the hooks agent's 11–15 ms figure. Their measurement used a .NET
`Process.Start` parent, which has comparable overhead to Node's. The Rust parent in §6.3 is not wrong, it is
simply measuring a cheaper caller than the one that exists.

### 6.5 Stage decomposition — where the end-to-end time goes

`stagehook` is `polis-hook` with `POLIS_STAGE` gating each phase. Round-robin interleaved, `n = 400`,
1 KiB payload, env-var endpoint. Marginal is the difference from the stage above.

| Stage | min | p50 | p90 | p99 | max | marginal p50 | marginal min |
|---|---|---|---|---|---|---|---|
| 0: `CreateProcess` + `exit(0)` only | 3.554 | 4.081 | 4.632 | 5.340 | 6.534 | — | — |
| 1: + panic hook + watchdog thread | 3.530 | 4.058 | 4.532 | 5.269 | 6.243 | −0.022 | −0.024 |
| 2: + read stdin, build frame | 3.667 | 4.102 | 4.510 | 5.153 | 5.615 | +0.044 | +0.136 |
| 3: + resolve endpoint | 3.673 | 4.201 | 4.723 | 5.355 | 7.215 | +0.098 | +0.006 |
| 4: **+ bind UDP socket (Winsock init)** | 4.471 | 5.025 | 5.575 | 6.137 | 9.834 | **+0.824** | **+0.798** |
| 5: + `send_to` = FULL HOOK | 4.520 | 5.077 | 5.580 | 6.421 | 7.076 | +0.052 | +0.049 |

The **watchdog thread is free** — its marginal cost is negative, i.e. below the noise floor. Winsock
initialisation is 82 % of everything the hook does beyond starting up.

### 6.6 Exec form vs shell form — a 24× decision

Claude Code runs a `command` hook through a shell unless the settings entry supplies `"args"`, in which
case the executable is spawned directly. `n = 250` after 15 warmups, 1 KiB payload.

| Registration form | min | p50 | p90 | p99 | max | vs exec |
|---|---|---|---|---|---|---|
| **exec form** — `"command": "polis-hook.exe", "args": ["--event","X"]` | 4.583 | **5.068** | 5.678 | 6.215 | 6.360 | 1× |
| shell form — Git Bash `-c "hook --event X"` | 28.748 | **30.820** | 32.990 | 35.686 | 41.604 | **6.1×** |
| shell form — PowerShell `-Command "& hook --event X"` | 113.064 | **123.941** | 130.790 | 157.616 | 328.389 | **24.5×** |

PowerShell is the default shell on a Windows box **without Git Bash**. A shell-form registration there would
cost the operator ~124 ms of blocked tool call per event. Exec form is not a micro-optimisation; it is the
difference between a usable and an unusable product on that configuration.

(No `cmd.exe` row: Claude Code's shell form is Git Bash, or PowerShell when Git Bash is absent. `cmd.exe`
is never on the path.)

### 6.7 Cold start

The first few spawns of a freshly-written binary are far more expensive than the steady state — 112.8 ms,
46.2 ms, 29.1 ms, 59.6 ms for the first four runs in the `connreset` probe, against a warm p50 of 4.7 ms.
That is antivirus scanning a new executable image. Every table above discards 15–25 warmup spawns, which is
the honest way to report steady state, but `polis install-hooks` should be aware that the first handful of
events after an install or upgrade are an order of magnitude slower, and should not be surprised by them.

### 6.8 Binary size

Release profile: `opt-level = "z"`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`,
`strip = "symbols"`, `overflow-checks = false`, `debug-assertions = false`, `incremental = false`.

| Binary | Size | Notes |
|---|---|---|
| **`polis-hook.exe`** | **148 480 bytes** | Reproducible across a `cargo clean` rebuild. |
| `noophook.exe` (`fn main() { exit(0) }`, identical profile) | 99 840 bytes | The Rust std floor on this target. |

**Dependency closure is empty.** `polis-hook`'s entire `Cargo.lock`:

```
version = 4

[[package]]
name = "polis-hook"
version = "0.1.0"
```

PRD §14's "no deps beyond libc + std" is satisfied with **std alone**. ~48 KB of the binary is Polis; the
other 100 KB is the Rust runtime floor and is not removable without `#![no_std]`, which would cost the
panic hook that §7 shows is load-bearing.

---

## 7. Two failure modes that would have shipped as blocking defects

Both were found by probing, not by reading, and both produce a **non-zero exit** — which per PRD §4.2
cancels the agent's tool call.

### 7.1 A panic exits `0xC0000409`, not 0

`panic = "abort"` on Windows terminates via `__fastfail`, and the parent observes exit code
`3221226505` = `0xC0000409`. Measured:

| Case | panic hook installed | exit code |
|---|---|---|
| `Vec` index out of bounds | no | **3221226505** |
| `Vec` index out of bounds | yes | 0 |
| `None.unwrap()` | no | **3221226505** |
| `None.unwrap()` | yes | 0 |
| `panic!("boom")` | no | **3221226505** |
| `panic!("boom")` | yes | 0 |
| clean `exit(0)` control | yes | 0 |

`std::panic::set_hook(Box::new(|_| std::process::exit(0)))` runs **before** the abort and converts any
panic anywhere in the binary — including inside std — into a clean exit 0. It is the last line of defence
behind the never-fail contract, it costs nothing measurable (§6.5 stage 1), and it must not be removed.

### 7.2 `std::env::args()` panics on a non-Unicode argv element

`args()` panics if any argument is not valid Unicode; `args_os()` does not. On Windows argv is UTF-16, so
an unpaired surrogate delivers one. Measured (the driver builds the `OsString` with
`OsStringExt::from_wide`, because Node sanitises lone surrogates and cannot reproduce this):

| Case | exit code |
|---|---|
| `env::args()`, valid argv, no panic hook | 0 |
| `env::args()`, **non-Unicode argv**, no panic hook | **−1073740791** (`0xC0000409`) |
| `env::args()`, non-Unicode argv, panic hook installed | 0 |
| `env::args_os()`, non-Unicode argv, no panic hook | 0 |

The hook uses `args_os()` and `var_os()` throughout. The panic hook covers it as well, so this is
defence in depth — but relying on the panic hook to catch a foreseeable input is not a design, and the
`args_os` version is not more expensive.

---

## 8. The safety matrix (PRD §16)

Every case asserts **observed exit code 0**. Any non-zero exit here cancels a real agent's tool call.
Run against the release binary, exit codes read from the parent.

| # | Case | Exit code | Result | Mechanism |
|---|---|---|---|---|
| 1 | **No listener present** (×50 consecutive) | `0` | **PASS** | Connectionless; `send_to` returns `Ok`, datagram discarded by the stack |
| 2 | **Receiver buffer flooded** (120 × 100 KiB at a 2 KiB-`SO_RCVBUF` socket that never reads) | `0` | **PASS** | Overflow is dropped at the *receiver*; the sender is unaffected |
| 3 | **Malformed / binary stdin** (9.5 KiB, every byte value, embedded NULs) | `0` | **PASS** | No parsing in the hook; bytes shipped verbatim |
| 4 | Truncated JSON with invalid UTF-8, unterminated | `0` | **PASS** | Payload is never decoded as text |
| 5 | **Oversized stdin 256 KiB** (the PRD cap) | `0` | **PASS** | Truncated to 60 KiB, `TRUNCATED` set, rest drained after send |
| 6 | Oversized stdin 1 MiB | `0` | **PASS** | as above |
| 7 | Oversized stdin 8 MiB | `0` | **PASS** | as above |
| 8 | Oversized stdin 32 MiB (Node parent) | `0` | **PASS** | as above; no parent-side stdin error |
| 9 | **Empty stdin** (pipe opened, zero bytes, closed) | `0` | **PASS** | Sends the 8-byte header with `len = 0` |
| 10 | **Stdin closed immediately** by the parent, nothing written | `0` | **PASS** | EOF on first read |
| 11 | Stdin is the `NUL` device — never a pipe at all | `0` | **PASS** | 4.96 ms |
| 12 | **Stdin held open forever** (writer never closes) | `0` | **PASS** | Watchdog fires at **505 ms**, not Claude Code's 5 s timeout |
| 13 | **Endpoint file missing**, no env var | `0` | **PASS** | Falls back to the compiled-in default port |
| 14 | Endpoint file corrupt: empty file | `0` | **PASS** | Rejected, default used |
| 15 | Endpoint file corrupt: binary garbage | `0` | **PASS** | Rejected, default used |
| 16 | Endpoint file corrupt: huge single line (1 MiB) | `0` | **PASS** | Read bounded to 128 bytes, rejected |
| 17 | Endpoint file corrupt: no colon | `0` | **PASS** | Rejected, default used |
| 18 | **Endpoint file garbage port**: `127.0.0.1:0` | `0` | **PASS** | Port 0 rejected explicitly |
| 19 | Endpoint file garbage port: `127.0.0.1:99999` | `0` | **PASS** | Out of `u16` range, rejected |
| 20 | Endpoint file garbage port: `127.0.0.1:-5` | `0` | **PASS** | Rejected |
| 21 | Endpoint file: unresolvable host `nonexistent.invalid:45177` | `0` | **PASS** | Rejected without a DNS lookup |
| 22 | Endpoint file: a path, `/run/user/1000/polis.sock` | `0` | **PASS** | Rejected |
| 23 | `POLIS_HOOK_ENDPOINT = ""` | `0` | **PASS** | Rejected, falls through to file |
| 24 | `POLIS_HOOK_ENDPOINT = "  "` | `0` | **PASS** | Rejected |
| 25 | `POLIS_HOOK_ENDPOINT = "garbage"` | `0` | **PASS** | Rejected |
| 26 | `POLIS_HOOK_ENDPOINT = "1.2.3.4"` (no port) | `0` | **PASS** | Rejected |
| 27 | `POLIS_HOOK_ENDPOINT = "999.999.999.999:1"` | `0` | **PASS** | Rejected |
| 28 | `POLIS_HOOK_ENDPOINT = "127.0.0.1:notaport"` | `0` | **PASS** | Rejected |
| 29 | `POLIS_HOOK_ENDPOINT = "[::1]:45177"` | `0` | **PASS** | IPv6 not supported by choice; rejected |
| 30 | **No `--event` argument at all** | `0` | **PASS** | Tag defaults to 0 (Unknown), still delivered |
| 31 | `--event` names an unknown event | `0` | **PASS** | Tag 0, still delivered; daemon re-derives from `hook_event_name` |
| 32 | `--event` with **no value after it** | `0` | **PASS** | No index panic |
| 33 | **`--event` value is not valid Unicode** (`args()` would abort) | `0` | **PASS** | `args_os()`; 5.91 ms |
| 34 | Extra and duplicate argv flags | `0` | **PASS** | First `--event` wins |
| 35 | **480 concurrent hooks**, 24-wide (one per core) | `0` | **PASS** | 984 ms total; ephemeral source ports do not collide |
| 36 | Any panic anywhere in the binary | `0` | **PASS** | Panic hook pre-empts `panic = "abort"` (§7.1) |

**36 / 36 pass. Zero non-zero exits observed in any configuration.**

The live receiver logged 493 datagrams and **0 receive errors** across the whole run.

**Two rows in PRD §16's list need editing.** §16 says *"Assert `polis-hook` exits 0 on: no listener present,
socket full, malformed stdin, oversized stdin, and `SIGPIPE`."* `SIGPIPE` is not a Windows concept and there
is no signal to catch; the equivalent hazards are rows 10–12 (stdin closed, stdin absent, stdin never
closed) and they belong in the list by name. Rows 30–34 (argv robustness) and row 36 (panic) are not in
§16's list at all and are the two that actually found defects.

---

## 9. Daemon-side receiver requirements

These are obligations on `polis-ingest`, derived from measurements here, not on the hook.

1. **`SO_RCVBUF` = 8 MiB.** At the default 65 536, a burst of 5 000 × 60 KiB datagrams lost 0.9 %; at 8 MiB
   it lost 0 %. Measured:

   | Receiver config | sent | send errors | received | loss |
   |---|---|---|---|---|
   | default 64 KiB rcvbuf, 1 KiB msgs | 5000 | 0 | 5000 | 0.0 % |
   | 8 MiB rcvbuf, 1 KiB msgs | 5000 | 0 | 5000 | 0.0 % |
   | **default 64 KiB rcvbuf, 60 KiB msgs** | 5000 | 0 | 4957 | **0.9 %** |
   | 8 MiB rcvbuf, 60 KiB msgs | 5000 | 0 | 5000 | 0.0 % |
   | 32 MiB rcvbuf, 60 KiB msgs | 5000 | 0 | 5000 | 0.0 % |

   PRD §13.1 asks for 500 events/sec sustained with zero drops. 5 000 datagrams delivered back-to-back with
   no loss clears that by an order of magnitude — and hooks are the *rare* channel, not the firehose.

2. **`SIO_UDP_CONNRESET = 0`** at startup (`WSAIoctl`, measured to succeed). Not strictly needed for a
   receive-only socket (§2.1 `[F]`), but it removes a whole error class from the ingest loop for one call.

3. **Bind `127.0.0.1:45177` exclusively**, no `SO_REUSEADDR`, and treat `AddrInUse` as "another Polis is
   running" — exit, do not fall back to another port (§5.3, §5.4).

4. **Validate every frame before trusting it.** The 8-byte header carries no magic, so the daemon's filter is:
   - `datagram_len >= 8`
   - `len + 8 == datagram_len`
   - `kind = tag & 0x7FFF_FFFF` is a known kind (0..=19)

   Anything else is dropped and counted, never fatal. This rejects essentially all accidental noise from an
   unrelated local process.

5. **Treat the argv tag as a hint, not truth.** `hook_event_name` inside the payload is authoritative; the
   tag exists so the daemon can route without parsing JSON on the ingest thread.

6. **A set `TRUNCATED` bit means notification-only.** Record the event, then backfill detail from the JSONL
   transcript (PRD §4.4). The original payload size is not recoverable from the wire.

7. **Read with a `recv_from` buffer of at least 65 536 bytes**, so a maximal frame is never silently
   truncated by the receive call itself.

---

## 10. Corrected budgets for PRD §4.2 and §13.1

PRD §4.2 states *"p99 wall time budget: **3ms**"* and §13.1 repeats it as
*"`polis-hook` p99 wall time | 3 ms"*. As a single end-to-end number this is unreachable on Windows, and
the reason is not in Polis's code: a binary whose entire body is `exit(0)` costs **10.6 ms p50 /
13.8 ms p99** when spawned from Node (§6.4).

**Replace the single row with three.**

| Metric | Budget | Basis |
|---|---|---|
| `polis-hook` **in-process** p99 (stdin read → framed → sent → exit) | **≤ 3 ms** | Measured worst p99 **1.234 ms**, worst single sample 1.561 ms, across 1 KiB / 100 KiB × listener / no-listener. 2.4× margin. This is the number Polis controls, and the one CI should gate on. |
| `polis-hook` **end-to-end** spawn→exit p99, **Windows** | **≤ 25 ms**, of which **≥ 13.8 ms is an OS + Node floor Polis does not control** | Measured 15.4–22.1 ms p99 from a Node parent; `exit(0)`-only baseline 13.8 ms p99. State the floor alongside the budget or the number reads as a Polis regression when it is a platform constant. |
| `polis-hook` end-to-end spawn→exit p99, **Linux / macOS** | **to be established on those CI legs** | **UNVERIFIED.** No non-Windows measurement was taken. Do not copy a Windows number across, and do not guess one — `fork`+`exec` and `CreateProcess` are not comparable. |

**Two further §13.1 rows are worth adding**, since they are measured and load-bearing:

| Metric | Budget | Basis |
|---|---|---|
| Hook registration form | exec form (`"args"`) — **shell form is a bug** | 6.1× (Git Bash) to 24.5× (PowerShell) slower, per event (§6.6) |
| `polis-hook` non-zero exit rate | **exactly 0**, no tolerance | §8; a non-zero exit cancels an agent's tool call |

**The corrected numbers strengthen §4.2's own argument.** §4.2 justifies keeping hooks rare by observing
that a 50 ms Node or Python hook is unaffordable. The measured floor is ~13 ms per event on Windows *no
matter how fast the binary is* — so the case for hooks-are-rare is stronger than the PRD assumed, and the
OTel channel carries even more of the weight. It also reframes the optimisation target: shaving the
in-process 0.9 ms is worth almost nothing, while a registration mistake (shell form) costs 25× more than
everything the binary does.

---

## 11. PRD corrections

Numbered for the editing pass. Items 1–4 are defects; 5–8 are additions the PRD does not currently require.

1. **§4.2 step 3 is not implementable.** *"one non-blocking `sendto()` on a Unix **datagram** socket at
   `$XDG_RUNTIME_DIR/polis.sock`"* → **one non-blocking `send_to()` on a loopback UDP socket, address
   resolved per §5.1.** `AF_UNIX`+`SOCK_DGRAM` fails at `socket()` on Windows with `WSAEAFNOSUPPORT`
   (re-verified, §1.1), and `XDG_RUNTIME_DIR` is unset here.

2. **§4.2 step 1's 256 KiB cap silently loses events.** A UDP datagram tops out at 65 507 payload bytes;
   65 508 fails with `WSAEMSGSIZE` (§4.1). → **Cap the send at 61 440 bytes and set bit 31 of the tag.**
   Read `MAX_PAYLOAD + 1` to detect overflow, and drain the remainder *after* the send so the agent's write
   always completes.

3. **§4.2 step 2 under-specifies the header.** → **`u32` tag then `u32` length, both little-endian; tag bits
   0..=30 are the event kind, bit 31 is `TRUNCATED`; `len` counts the payload bytes in this datagram, not
   the original stdin size.** Without a stated endianness this is a latent cross-crate bug.

4. **§4.2 and §13.1's 3 ms p99 is unreachable as an end-to-end number.** → Split into in-process and
   spawn budgets per §10, with the Windows floor documented as an OS+runtime constant.

5. **§4.2 must require the panic hook.** Without `std::panic::set_hook(… exit(0))`, any panic exits
   `0xC0000409` and cancels the agent's tool call (§7.1). The PRD says "Never exit non-zero" but does not
   say how; on Windows with `panic = "abort"` that is not automatic.

6. **§4.2 must require `args_os()`/`var_os()`, never `args()`/`var()`.** `args()` aborts on a non-Unicode
   argv element (§7.2).

7. **§4.2 should require a watchdog.** A hook stuck in `read(stdin)` blocks a real tool call for Claude
   Code's full 5 s hook timeout. A 500 ms self-limiting thread caps it at a tenth of that and is free
   (§6.5). Measured firing at 505 ms / 518 ms.

8. **§4.2's "Never a FIFO" rationale needs a Windows sentence.** The Windows equivalent hazard is a
   **named pipe** (`\\.\pipe\...`), where `CreateFile` blocks pending a server instance. The warning is
   right; the mechanism name is Unix-only. (Also noted in `hooks-schema.md` §9.5.)

9. **§16's hook-safety list should replace `SIGPIPE`** — not a Windows concept — **with: stdin closed
   immediately, stdin absent (`NUL`), stdin never closed, non-Unicode argv, missing/corrupt endpoint file,
   and induced panic.** Rows 30–36 of §8 are where the two real defects were found; `SIGPIPE` would have
   found neither.

10. **§14's "no deps beyond libc + std" can be tightened to "std only".** Measured: `polis-hook`'s
    `Cargo.lock` contains exactly one package, itself (§6.8).

Cross-referenced from `hooks-schema.md`, not re-derived here, and still binding on this component:
`WorktreeCreate` must never be registered; `PreToolUse` must be matcher-narrowed to
`^(Edit|Write|NotebookEdit)$`; hooks must be registered in exec form with an explicit `"timeout": 5`;
`OTEL_*` variables are stripped from hook processes, so the hook must not look for the daemon there.

---

## 12. Reference implementation

Copy verbatim into `polis-hook/`. Compiles clean and passes its unit tests on
`rustc 1.94.0` / `x86_64-pc-windows-msvc`; every number in this document was produced by this exact source.

### `polis-hook/Cargo.toml`

```toml
[package]
name = "polis-hook"
version = "0.1.0"
edition = "2021"

# Deliberately empty. polis-hook links std only, on every platform.
[dependencies]

[profile.release]
opt-level = "z"
lto = "fat"
codegen-units = 1
panic = "abort"
strip = "symbols"
overflow-checks = false
debug-assertions = false
incremental = false
```

### `polis-hook/src/main.rs`

```rust
//! polis-hook — the Polis hook transport binary (PRD §4.2).
//!
//! Contract:
//!   * never blocks the agent
//!   * never exits non-zero, for any reason, ever
//!   * an absent daemon is a normal condition, not an error
//!   * no dependencies beyond `std`, on every platform
//!   * no JSON parsing: the payload is shipped as opaque bytes
//!
//! Wire format — one UDP datagram to a loopback address:
//!
//!   offset 0  u32 LE  tag   bits 0..=30 event kind, bit 31 = TRUNCATED
//!   offset 4  u32 LE  len   payload bytes in THIS datagram
//!   offset 8  ..len   payload (raw hook stdin, unparsed)
//!
//! Little-endian is fixed by decision: this is a same-machine loopback
//! protocol, every target Polis supports is little-endian, and LE lets both
//! ends use `to_le_bytes`/`from_le_bytes` with no byte swap.
//!
//! See docs/verified/hook-ipc.md for the measurements that fix every constant
//! and for the daemon-side receiver requirements.

use std::io::Read;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

/// 8-byte header, exactly as PRD §4.2 step 2 specifies.
const HEADER_LEN: usize = 8;

/// Largest payload that still fits one IPv4 UDP datagram with the header.
/// Measured hard limit on this machine: 65507 bytes of payload, 65508 ->
/// WSAEMSGSIZE. 60 KiB + 8 = 61448 leaves comfortable slack.
///
/// This supersedes PRD §4.2's 256 KiB stdin cap for the *send*: a 256 KiB
/// datagram cannot exist, and an over-cap send fails with EMSGSIZE, silently
/// losing the whole event.
const MAX_PAYLOAD: usize = 60 * 1024; // 61440

/// Set in the tag's high bit when stdin exceeded MAX_PAYLOAD. The daemon treats
/// a truncated event as notification-only and backfills detail from the JSONL
/// transcript (PRD §4.4).
const TAG_TRUNCATED: u32 = 0x8000_0000;

/// Hard ceiling on total process lifetime. A blocking hook stalls a real tool
/// call; Claude Code's own timeout is 5 s, which is 5 s of stall. This caps the
/// worst case at half a second even if stdin never reaches EOF.
const WATCHDOG_MS: u64 = 500;

/// Last-resort endpoint when neither the env var nor the endpoint file is
/// usable. Unregistered with IANA; picked once and frozen. Deliberately below
/// the OS ephemeral range (measured 49152+ here) so it can never be taken by an
/// anonymous client socket.
const DEFAULT_PORT: u16 = 45177;

/// Explicit override, injected by the daemon into agents it launches itself.
const ENDPOINT_ENV: &str = "POLIS_HOOK_ENDPOINT";

fn main() {
    // Belt and braces on top of `panic = "abort"`: the panic hook runs *before*
    // the abort, so this converts any panic anywhere into a clean exit(0).
    // Without it a panic exits 0xC0000409 and cancels the agent's tool call.
    std::panic::set_hook(Box::new(|_| std::process::exit(0)));

    // Nothing below may outlive this.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(WATCHDOG_MS));
        std::process::exit(0);
    });

    deliver();

    // The only intended exit in the program.
    std::process::exit(0);
}

/// Everything fallible lives here. Every error is swallowed on purpose.
fn deliver() {
    // ---- 1. event-kind tag from argv --------------------------------------
    // Passed by the settings.json hooks block as `--event <Name>` (exec form,
    // no shell). The hook never parses JSON to learn which event it is.
    // `args_os` and not `args`: `args()` panics on non-UTF-8 arguments.
    let mut tag: u32 = 0;
    let mut want = false;
    for a in std::env::args_os().skip(1) {
        if want {
            tag = a.to_str().map(event_tag).unwrap_or(0);
            break;
        }
        want = a == *"--event";
    }
    tag &= !TAG_TRUNCATED;

    // ---- 2. read stdin, capped --------------------------------------------
    let mut buf: Vec<u8> = Vec::with_capacity(HEADER_LEN + 16 * 1024);
    buf.extend_from_slice(&[0u8; HEADER_LEN]);

    let stdin = std::io::stdin();
    let mut locked = stdin.lock();
    {
        // One byte past the cap, so overflow is detectable without a second
        // syscall. A read error mid-stream is fine: ship whatever arrived.
        let mut capped = (&mut locked).take((MAX_PAYLOAD + 1) as u64);
        let _ = capped.read_to_end(&mut buf);
    }

    let truncated = buf.len() > HEADER_LEN + MAX_PAYLOAD;
    if truncated {
        buf.truncate(HEADER_LEN + MAX_PAYLOAD);
        tag |= TAG_TRUNCATED;
    }

    let payload_len = (buf.len() - HEADER_LEN) as u32;
    buf[0..4].copy_from_slice(&tag.to_le_bytes());
    buf[4..8].copy_from_slice(&payload_len.to_le_bytes());

    // ---- 3. resolve endpoint ----------------------------------------------
    let endpoint = resolve_endpoint();

    // ---- 4. exactly one non-blocking datagram send ------------------------
    // bind(127.0.0.1:0) takes an ephemeral source port: a fixed source port
    // would collide across concurrent hook processes.
    //
    // Binding the SOURCE to 127.0.0.1 is a security control, not an accident:
    // measured, a 127.0.0.1-bound socket cannot reach a non-loopback address at
    // all. Hook payloads contain source code, so a tampered endpoint file must
    // not be able to ship them off the box.
    //
    // Unconnected `send_to`, never `connect()` + `send()`: a *connected* UDP
    // socket on Windows latches ICMP port-unreachable feedback and fails a
    // later call with WSAECONNRESET. An unconnected socket has no connection
    // state to attach that error to.
    if let Ok(sock) = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)) {
        // Redundant for a single send into an empty socket buffer, but it is
        // the contract: under no circumstance may this call park.
        let _ = sock.set_nonblocking(true);
        // Result deliberately discarded. WouldBlock, ConnectionReset,
        // MessageSize, anything at all: drop the event and go.
        let _ = sock.send_to(&buf, endpoint);
    }

    // ---- 5. drain the rest of stdin, AFTER the send -----------------------
    // Exiting with bytes still in the pipe makes the agent's write fail with
    // EPIPE / ERROR_NO_DATA, which is the class of disruption this binary
    // exists to avoid. Draining after the send means an oversized payload
    // delays nothing that matters, and the watchdog bounds it.
    if truncated {
        let _ = std::io::copy(&mut locked, &mut std::io::sink());
    }
}

/// env var -> endpoint file -> compiled-in default. Cheapest first.
fn resolve_endpoint() -> SocketAddrV4 {
    if let Some(v) = std::env::var_os(ENDPOINT_ENV) {
        if let Some(a) = v.to_str().and_then(parse_endpoint) {
            return a;
        }
    }
    if let Some(p) = endpoint_file_path() {
        // Bounded read: a corrupt or hostile endpoint file must not be able to
        // make us read a gigabyte. 128 bytes is far more than
        // `255.255.255.255:65535\n` could ever need.
        if let Ok(f) = std::fs::File::open(&p) {
            let mut head = [0u8; 128];
            let mut n = 0usize;
            let mut r = f.take(128);
            while n < head.len() {
                match r.read(&mut head[n..]) {
                    Ok(0) | Err(_) => break,
                    Ok(k) => n += k,
                }
            }
            // First line only; the daemon may append diagnostics after it.
            // Lossy on purpose: a corrupt file must not be able to bail us out
            // of the lookup, it must just fail to parse.
            let s = String::from_utf8_lossy(&head[..n]);
            if let Some(a) = s.lines().next().and_then(parse_endpoint) {
                return a;
            }
        }
    }
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, DEFAULT_PORT)
}

/// Strictly a literal IPv4 loopback `a.b.c.d:port`.
///
/// Deliberately NOT `ToSocketAddrs`: a hostname would put a DNS lookup on the
/// agent's critical path — measured at milliseconds for a failing name, most of
/// the entire in-process budget — for a value that is always 127.0.0.0/8.
/// Rejecting non-loopback is also what confines a tampered endpoint file.
fn parse_endpoint(s: &str) -> Option<SocketAddrV4> {
    let a: SocketAddrV4 = s.trim().parse().ok()?;
    if a.port() != 0 && a.ip().is_loopback() {
        Some(a)
    } else {
        None
    }
}

/// `%LOCALAPPDATA%\polis\endpoint` — per-user, ACLed to the user by Windows,
/// and present on every Windows install. Not `%TEMP%`: world-writable in
/// multi-user configurations.
#[cfg(windows)]
fn endpoint_file_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let mut p = std::path::PathBuf::from(base);
    p.push("polis");
    p.push("endpoint");
    Some(p)
}

/// `$XDG_RUNTIME_DIR/polis/endpoint`, falling back to `$XDG_STATE_HOME` and
/// then `~/.local/state` — XDG_RUNTIME_DIR is unset in plenty of environments
/// (it is unset in this one), and a hook that only looks there finds nothing.
#[cfg(not(windows))]
fn endpoint_file_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("XDG_STATE_HOME").map(std::path::PathBuf::from))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })?;
    Some(base.join("polis").join("endpoint"))
}

/// The event-kind table. Kept identical to the `--event` names written by
/// `polis install-hooks`; the daemon holds the mirror image of this match.
/// An unknown name is tag 0 (Unknown) and is still delivered — the daemon
/// re-derives the true kind from `hook_event_name` in the payload, which is
/// authoritative. The argv tag is a routing hint, not a source of truth.
fn event_tag(name: &str) -> u32 {
    match name {
        "SessionStart" => 1,
        "SessionEnd" => 2,
        "PreToolUse" => 3,
        "PostToolUseFailure" => 4,
        "PermissionRequest" => 5,
        "SubagentStart" => 6,
        "SubagentStop" => 7,
        "TaskCreated" => 8,
        "TaskCompleted" => 9,
        "Stop" => 10,
        "StopFailure" => 11,
        "TeammateIdle" => 12,
        "Notification" => 13,
        "Elicitation" => 14,
        "ElicitationResult" => 15,
        "WorktreeRemove" => 16,
        "CwdChanged" => 17,
        "PreCompact" => 18,
        "PostCompact" => 19,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_datagram_fits_ipv4_udp() {
        // Measured hard limit: 65507 payload bytes; 65508 -> WSAEMSGSIZE.
        assert!(HEADER_LEN + MAX_PAYLOAD <= 65507);
    }

    #[test]
    fn truncated_bit_cannot_be_forged_by_a_kind() {
        for name in ["SessionStart", "PostCompact", "nonsense", ""] {
            assert_eq!(event_tag(name) & TAG_TRUNCATED, 0);
        }
    }

    #[test]
    fn event_tags_are_unique_and_in_range() {
        let names = [
            "SessionStart", "SessionEnd", "PreToolUse", "PostToolUseFailure",
            "PermissionRequest", "SubagentStart", "SubagentStop", "TaskCreated",
            "TaskCompleted", "Stop", "StopFailure", "TeammateIdle",
            "Notification", "Elicitation", "ElicitationResult",
            "WorktreeRemove", "CwdChanged", "PreCompact", "PostCompact",
        ];
        let mut seen: Vec<u32> = names.iter().map(|n| event_tag(n)).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), names.len(), "duplicate event tag");
        assert_eq!(event_tag("WorktreeCreate"), 0, "must never be registered");
    }

    #[test]
    fn rejects_everything_that_is_not_literal_loopback_v4() {
        for bad in [
            "",
            "  ",
            "garbage",
            "1.2.3.4",
            "127.0.0.1",
            "127.0.0.1:0",
            "127.0.0.1:99999",
            "127.0.0.1:-5",
            "127.0.0.1:notaport",
            "localhost:45177",           // would cost a DNS lookup
            "nonexistent.invalid:45177", // would cost a FAILING DNS lookup
            "192.168.1.5:45177",         // off-box
            "0.0.0.0:45177",
            "[::1]:45177", // v6 loopback: not supported, by choice
            "/run/user/1000/polis.sock",
            "127.0.0.1:45177:45178",
        ] {
            assert!(parse_endpoint(bad).is_none(), "should reject {bad:?}");
        }
        assert_eq!(parse_endpoint("127.0.0.1:45177").unwrap().port(), 45177);
        assert!(parse_endpoint(" 127.0.0.9:1 \n").is_some());
    }
}
```

```
running 4 tests
test tests::event_tags_are_unique_and_in_range ... ok
test tests::max_datagram_fits_ipv4_udp ... ok
test tests::rejects_everything_that_is_not_literal_loopback_v4 ... ok
test tests::truncated_bit_cannot_be_forged_by_a_kind ... ok

test result: ok. 4 passed; 0 failed
```

---

## 13. Reproducing the measurements

Probe workspace, all crates zero-dependency:

| Crate / file | Purpose |
|---|---|
| `polis-hook/` | The binary under test — the source in §12. |
| `timedhook/` | Byte-identical to `polis-hook` plus one `Instant` at `main` entry and one elapsed print *after* the measured window. Produces §6.2. |
| `stagehook/` | `polis-hook` with `POLIS_STAGE` gating each phase. Produces §6.5. |
| `noophook/` | `fn main() { exit(0) }`, identical release profile. The spawn floor. |
| `panicprobe/` | Panic and non-Unicode-argv behaviour. Produces §7. |
| `probe/src/bin/harness.rs` | Everything else: `facts`, `facts2`, `filecost`, `wirecheck`, `bench`, `intime`, `stages`, `shellcost`, `connreset`, `discovery`, `safety`. |
| `probe/src/bin/afunix.rs` | Raw `socket()` calls: `AF_UNIX` availability (§1.1). |
| `probe/src/bin/bindprobe.rs` | Singleton-lock and ephemeral-range semantics (§5.3). |
| `probe/src/bin/argvprobe.rs` | Delivers a genuinely non-Unicode argv via `OsStringExt::from_wide` (§7.2). |
| `nodebench.js`, `nodesafety.js`, `panicdrive.js` | Node-parent spawn cost (§6.4) and parent-side stdin safety (§4.3). |

```
harness facts                                  # UDP behaviour, datagram limits, exclusivity
harness facts2                                 # SO_SNDBUF, loss vs SO_RCVBUF, DNS cost, loopback confinement
harness filecost                               # endpoint-file read cost
harness connreset  <polis-hook>                # the WSAECONNRESET proof
harness discovery  <polis-hook> 300            # endpoint discovery correctness + cost
harness wirecheck  <polis-hook>                # header layout on the wire
harness intime     <timedhook> <bytes> 400 <0|1>
harness bench      <exe> <bytes> 400 <0|1>
harness stages     <stagehook> 1024 400
harness shellcost  <polis-hook> 250
harness safety     <polis-hook>                # the §8 matrix
node nodebench.js  <exe> <bytes> 400 <0|1>
node nodesafety.js <polis-hook>
```

Raw output is kept alongside the sources as `facts.out`, `facts2.out`, `filecost.out`, `connreset.out`,
`discovery.out`, `wirecheck.out`, `intime.out`, `bench_rust.out`, `bench_node.out`, `stages.out`,
`shellcost.out`, `safety.out`, `nodesafety.out`, `afunix.out`, `bindprobe.out`, `argv.out`, `panic.out`.

**Two methodology notes for whoever re-runs this.** Interleave variants round-robin rather than in blocks,
or background load will be attributed to whichever variant was unlucky — this produced a 114 ms p99 for
`exit(0)` and negative marginal costs on the first attempt. And discard 15–25 warmup spawns: a
freshly-built executable's first runs are 6–24× slower while antivirus scans the image (§6.7).
