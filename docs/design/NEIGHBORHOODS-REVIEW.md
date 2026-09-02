# Neighborhood data layer — independent review

Reviewer: a second agent, working only in `polis-repo`. Run against the
operator's own eight repositories in `C:/coding` plus full-depth clones of
Django, Neovim and Click. Nothing here was taken from the implementer's report;
every number below was produced by running the code.

**Verdict: yellow.** The partition and the kinds are good and I would ship them.
The descriptions are not what was promised. The estimate that
derived-from-repo would get "~70 % of the value" was wrong: on the operator's
own repositories it gets about a quarter, and the shortfall is structural, not a
matter of more rules.

---

## 1. Build, determinism, network

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets` | **0 warnings** |
| `cargo test --workspace` | **951 passed, 0 failed** |
| Golden snapshots | green, and correctly **not** regenerated |

### The goldens were right not to move

`tests/golden/{hamlet,town,town-in-use}.snap` are `polis-layout`'s `m1_gate`
snapshots. The neighborhood work touched no file in `polis-layout` and
`RepoIndex::districts` still returns every directory, which is the granularity
the layout places plots at. Neighborhoods are a *labelling and colour*
partition layered over the same geometry. So there was nothing to regenerate,
and `m1_gate`'s own `a_stale_golden_and_a_nondeterministic_one_are_told_apart`
still passes. This is the honest answer, not an omission.

My own changes (below) also leave the goldens untouched, verified by running
them.

### Determinism, measured rather than asserted

Byte-identical output required across processes, across debug/release, and
across input permutations (PRD §7.4).

| Check | Coverage | Result |
|---|---|---|
| Two separate processes, release | 8 real repositories | identical |
| Debug binary vs release binary | 8 real repositories | identical |
| Input permutation into `Neighborhoods::from_files` — reversed, stride-7, stride-97, stride-1009, halves-swapped | 8 repositories × 5 permutations | identical |
| Cold description cache vs warm cache | 8 repositories | descriptions identical |

The one apparent failure was `agentolis` itself, and it was real input drift:
`target/` grew by 922 files because I ran a build between the two passes. Every
civic district was identical; re-run back to back with no build in between, it
matches byte for byte.

Static audit of `kinds.rs`, `neighborhoods.rs`, `describe.rs`: **no**
`HashMap`, `HashSet`, `RandomState`, `DefaultHasher`, `ahash`, `rand`,
`SystemTime` or `Instant::now`. `BTreeMap` throughout. The only `read_dir` in
the crate (`tree.rs:632`) sorts both its file list and its directory list before
use. The description cache key is `imports::content_digest`, a hand-written
128-bit FNV pair, not a seeded hasher.

**One latent trap, not currently firing.** `Neighborhoods` serialises its
`NeighborhoodStats`, which embeds `DescribeStats { files_read, cache_hits, … }`.
Those counters legitimately differ between a cold run (35 read, 2 hits) and a
warm one (0 read, 37 hits). The *descriptions* are identical, so nothing is
wrong today — but the moment anything golden-files a serialised `Neighborhoods`,
cache warmth flips bytes and looks exactly like nondeterminism. This project has
already been burned once by that shape. Remedy is one line
(`#[serde(skip)]` on the counters, or exclude them from any snapshot); I did not
apply it because the serialised shape is read by crates another agent is
mid-edit on.

### Nothing leaves the box

`polis-repo/Cargo.toml` and `Cargo.lock` were **not touched** by this work — no
dependency was added, so no HTTP client could have been. Grepping the three new
modules for `reqwest`, `hyper`, `ureq`, `curl`, `TcpStream`, `UdpSocket`,
`std::net`, `socket`, `openai`, `anthropic` finds nothing but two doc comments
that mention the string `https://` while explaining URL sanitising. There is no
model call, local or remote. PRD §2 holds.

The description cache is written to `%LOCALAPPDATA%\polis\describe\<key>.json`,
outside the repository. Nothing writes into a scanned repository; every
`fs::write` in the three modules is inside a `#[cfg(test)]` block.

---

## 2. Are the kinds right?

Spot-checked by opening the directories.

| District | Called | On disk | Verdict |
|---|---|---|---|
| `qurio-toolset/tests/services` | test | 172 × `*.test.js` | correct |
| `qurio-toolset/src/styles/generated` | vendored | generated design tokens | correct |
| `qurio-toolset/.playwright-mcp` | ~~data~~ | console logs + page dumps | **fixed → vendored** |
| `qurio-toolset/electron/signing/apple` | config | `.pem`, `.cer`, `.p12`, `.key` | correct |
| `stickingplacebooks/emulator-data/.../media` | assets | book cover images | correct |
| `stickingplace/.astro` | ~~source~~ | Astro's generated cache | **fixed → vendored** |
| `biwt/.playwright-mcp` (106 files) | ~~config~~ | browser-automation dumps | **fixed → vendored** |
| `django/*/locale` | data | `.po` / `.mo` catalogues | correct |
| `neovim/runtime/syntax` | source | 800 `.vim` scripts | correct |

The implementer's two claimed fixes hold up independently: Neovim's `.vim`
files are `source`, and Django's ten `contrib` apps read `source` with their
`locale` trees beside them as their own `data` districts. The contrast-split
rule is doing real work.

### Misclassifications still standing

**1. `vc-tower/vcsheet-scraper` — 13 058 scraped files misread, and it wrecks
that repository's map.** This is the worst finding in the review.

```
vcsheet-scraper/partners        config  4418   ← 4 418 scraped *.json
vcsheet-scraper/partners_html   source  4418   ← 4 418 scraped *.html
vcsheet-scraper/funds           config  2111
vcsheet-scraper/funds_html      source  2111
```

`.json` defaults to `Config` and `.html` is a code extension, so a web scrape's
output reads as configuration and source. Because those 13 058 files count as
*civic*, they set the sizing budget: ceiling 1 064, floor 54. The operator's
entire Next.js application — `web/src`, 100 files — therefore never splits and
lands as **one blob**. That is precisely the complaint that started this work,
reproduced on the operator's own repository.

Measured remedy, with the four trees declared vendored in
`.polis/neighborhoods.json`:

| | as shipped | with the config line |
|---|---:|---:|
| civic files | 13 299 | 241 |
| ceiling / floor | 1 064 / 54 | 20 / 6 |
| civic districts | 8 | **16** |
| what you see | `partners`, `partners_html`, `funds`, `funds_html`, `web/src` (100) | `lib/orchestrator`, `lib/tools`, `app/api`, `lib/firestore`, `components/dashboard`, `web/scripts`, … |

I did **not** flip `.json → Data` globally. It would be a large semantic change
across every repository, ADR-0085 chose the current default deliberately, and
the named-manifest table (`package.json`, `tsconfig.json`, `firebase*`,
`*.config.*`) is what keeps real configuration correct. The right fix is
step 1 of §8.

**2. TypeScript build output reads as source.** `stickingplacebooks/functions/lib`
(54 files, "50 % source, 50 % vendored") and `Squigglo/functions/lib/pipeline`
(14 files) are `tsc` output — `.js` next to `.js.map`. The `.js.map` half is
correctly vendored by the generated-suffix rule; the `.js` half is not.
`lib/` cannot be added to the industrial list (it is source in most
repositories); the signal is the `.js` + `.js.map` pairing.

**3. Emulator export blobs are `unknown`.**
`stickingplacebooks/emulator-data/emulator-seed-2026-03-29-154659`, 39 files,
100 % unknown. Honest, and the honesty is the design — but it is a 39-file
district drawn in the "no rule matched" hue.

**4. One-file residual districts.** After `polis-app/src` is promoted, what is
left of `polis-app` is a `Cargo.toml`, so `agentolis` has districts named
`polis-events`, `polis-ingest`, `polis-render` that are one config file each.
`stickingplacebooks` has `emulator-data` and `emulator-data/storage_export` at
one file each. Harmless for placement, noise on a legend.

**5. Neovim's root district is `unknown`** (54 files) — the civic square, in
the "no rule matched" colour.

### Accounting inconsistency (cosmetic, worth knowing)

The header line and the kind line disagree slightly on some repositories —
`stickingplacebooks` reports `civic 1002, vendored 80064` but `vendored=80092`.
The 28-file gap is minified and source-map files inside *civic* directories:
`kind_of` calls `IndustrialRules::is_industrial_file` (which matches generated
*suffixes*), while the sizing budget asks `is_industrial_dir`. Both answers are
defensible — a `.min.js` inside `src/` should be drawn dull but should not
become its own district — but ADR-0085's claim that the two "cannot disagree"
is not literally true.

---

## 3. Is the granularity right?

**Yes.** This is the strongest part of the work.

| repository | files | civic | top-level (old) | neighborhoods | every dir |
|---|---:|---:|---:|---:|---:|
| qurio-toolset | 90 459 | 1 571 | 70 | **55** | 13 121 |
| stickingplacebooks | 81 066 | 1 002 | 49 | **31** | 12 036 |
| Squigglo | 75 760 | 240 | 36 | **26** | 10 602 |
| vc-tower | 74 806 | 13 299 | 24 | **11** | 8 262 |
| biwt | 62 430 | 223 | 35 | **20** | 9 471 |
| agentolis | 46 237 | 143 | 22 | **19** | 2 932 |
| stickingplace | 38 612 | 306 | 25 | **14** | 4 875 |
| qurio-networked | 2 433 | 17 | 10 | **3** | 289 |
| django | 7 086 | 7 014 | 28 | **68** | 3 275 |
| neovim | 3 918 | 3 890 | 39 | **48** | 234 |
| click | 166 | 166 | 15 | **10** | 25 |

3 → 68 districts, none of it a target count. Holding vendored trees out of the
sizing budget is the load-bearing decision: `qurio-toolset` is 98 %
`node_modules`, and letting that set the scale would have collapsed the
operator's own 1 571 files into one district.

Could you point at the map and say "that's the auth code"? On `qurio-toolset`,
yes — `components/GmailWindow`, `components/ChatWindow`, `services/memory`,
`services/integrations`, `src/services/userModel`, `tests/services`,
`components/MediaWindow`. That is a recognisable city. The size distribution is
223 → 2 with no cliff, not one blob and twenty crumbs.

Two naming weaknesses, both minor:

* Nesting is invisible in the label. `src/components` (146 files) and
  `components/custom` (75) read as peers; the second is inside the first. The
  `parent`/`children` fields carry the truth, so this is the renderer's to fix.
* The two-component floor has a 30-character escape hatch, so
  `src/components/AttentionAnalyzerPanel` is labelled `AttentionAnalyzerPanel`
  while its fifteen siblings are labelled `components/…`. Unambiguous, but
  inconsistent in a legend.

`vc-tower` is the exception, and §2 explains why.

---

## 4. Are the descriptions useful, or filler?

Being harsh, as asked. Across the operator's eight repositories, **179
districts**:

| | count | share |
|---|---:|---:|
| no description at all | 83 | 46 % |
| synthesised inventory ("most imported: X", "N files named Y") | 58 | 32 % |
| prose quoted from the repository | **38** | **21 %** |

`agentolis` supplies 15 of those 38 — it is Polis itself, every crate has a
`//!` and a `Cargo.toml` description. **Excluding the dogfood repository: 23
prose descriptions across 160 districts, 14 %.**

Now grading those 23 by hand.

**Genuinely informative — tells you something the label does not (10):**

| district | description |
|---|---|
| `qurio-toolset/functions` | Cloud Functions for Qurio Gmail Integration |
| `qurio-toolset/electron` | Qurio desktop app with native browser window overlays |
| `qurio-toolset/services/integrations` | Gmail Service — Frontend integration with Firebase Functions |
| `qurio-toolset/src/config` | Firebase Configuration and Initialization |
| `qurio-toolset/services/tools` | Tool Registry · *central registry for all AI agent tools* |
| `qurio-toolset/services/adapters` | Tool Format Converters · *converts tool definitions between provider formats* |
| `stickingplacebooks/scraper` | This standalone tool scrapes book data from the existing Sticking Place Books WordPress site… |
| `vc-tower/` | Investor enrichment orchestrator with a step-by-step debug UI. |
| `vc-tower/vcsheet-scraper` | Standalone Python scraper for vcsheet.com. |
| `stickingplace/pages/alexander-mackendrick` | Alexander Mackendrick was one of Britain's greatest film directors. |

**Restates the folder name — filler (9):** `services/settings` → "The settings
entry contract."; `services/memory` → "Memory System — Main Export";
`services/dropZoneService` → "Drop Zone Service"; `components/a2ui` → "A2UI
Renderer"; `lib/data` → "Data layer exports"; `lib/demo` → "Demo mode
utilities"; `scraper/src` → "Types for web scraper"; `functions/src` →
"Firebase Cloud Functions"; `Squigglo/functions` → "Cloud Functions for
Firebase".

`services/dropZoneService` described as "Drop Zone Service" is exactly the
"utils described as Utility functions" failure ADR-0087 says it refuses. The
`says_nothing_new` guard only runs on README *headings*; a doc comment that
restates the directory name walks straight through.

**Misleading — one file's doc comment standing in for the whole district (4):**

| district | files | described by | as |
|---|---:|---|---|
| `src/services` | 223 | `WindowManager.js` | "Centralized Window Management Service" |
| `src/hooks` | 46 | `useDropZone.js` | "useDropZone Hook" |
| `src/services/userModel` | 20 | `threadIntegrationService.js` | "threadIntegrationService — the L2 tier…" |
| `components/GmailWindow` | 7 | `ContactAutocomplete.jsx` | "the Gmail composer's To/Cc/Bcc field" |

`src/services` is 223 files covering memory, Gmail, tools, settings, adapters
and the user model. Labelling that quarter of the city "Window Management" is
worse than labelling it nothing.

**So: 10 of 23 prose descriptions are informative — 10 out of 160 districts, 6 %.**

The 58 inventory lines mostly restate the monument, and PRD §8 already says the
monument is "always labelled at every zoom". "most imported: `BaseWindow.jsx`"
next to a building already labelled `BaseWindow.jsx` is the same fact twice. It
earns its place in a drill-down panel and not on the map.

**Was "~70 % of the value" true? No.** It is roughly 25 % — one informative
sentence for every four districts that have any text at all, and one for every
sixteen districts on the map. The gap is not missing rules. It is that most
directories in a working repository *contain no sentence saying what they are*.
`biwt` has 20 districts and **zero** prose. `stickingplace` has 14 and one.
`Squigglo` has 26 and one. No extractor can quote what is not written.

---

## 5. Safety

Clean. Every description produced across eleven repositories was scanned for
credential prefixes (`sk-`, `AKIA`, `AIza`, `ghp_`, `xox*`), PEM headers,
assignments to `password`/`token`/`api_key`/`secret`, high-entropy strings,
absolute paths, `C:\`, `/Users/`, `/home/`, the operator's name and email.
**Zero hits.**

The describer's read set is closed and small by construction: `README*`,
`index.md`, `OVERVIEW.md`, `package.json`, `Cargo.toml`, `pyproject.toml`,
`ANCHOR_NAMES`, and the district's monument when it has a tree-sitter language.
`qurio-toolset/electron/signing/apple` contains a real Apple private key
(`AuthKey_*.p8`), `devid.key`, a `.p12` and a `p12-password.txt`; none of those
is in the read set, none was opened, and the district's description is `None`.

The `Neighborhood.description.detail` field does embed repo-relative paths
("Most imported here: `src/app/admin/books/id/BookContext.tsx`"). Not a secret,
but if the detail is ever drawn onto a shareable image the renderer should know
it is showing a path.

---

## 6. What I changed

Five fixes, all in `polis-repo`. Workspace green after each: fmt clean, clippy
**0 warnings**, 951 tests passing, goldens untouched and still green.

**A. A linter pragma is not a description.** `qurio-toolset/components/landing`
was described, on the map, as **"eslint-disable"** — a leading
`/* eslint-disable */` block comment sits exactly where a module doc comment
sits, and tree-sitter was right to hand it over. Added
`SanitiseReject::Boilerplate` and a `PRAGMA_PREFIXES` table
(`eslint-*`, `prettier-ignore`, `ts-nocheck`, `noqa`, `pylint:`, `coding:`, …),
prefix-matched after markup stripping so `# eslint-disable` and
`-*- coding: utf-8 -*-` are caught too.

**B. A project generator's README is not a description of your project.** Four
of the operator's eight repositories were described by scaffolding text, and
always on the district that can least afford it — the **root**, PRD §8's civic
square:

* `stickingplacebooks/`, `Squigglo/`, `vc-tower/web` → "This is a Next.js project bootstrapped with create-next-app."
* `qurio-toolset/` → "This template provides a minimal setup to get React working in Vite with HMR and some ESLint rules."

Added a short, verbatim `BOILERPLATE_PHRASES` list, rejected the same way a
secret is — whole, so the next source gets its turn. A sentence that merely
*mentions* eslint or Vite still survives; there is a test for that.

**C. Bumped the description cache version to 2.** The cache is keyed on file
bytes, not on the rules that read them, so without this a warm cache would keep
serving "eslint-disable" — and only on machines that had run before, which is
the worst way to discover a rule change. The constant now says so.

**D. A monument that only repeats the district's own name says nothing.**
`components/ChatWindow` was described as "most imported: `ChatWindow.jsx`", and
`src/types` as "most imported: `index.ts`". `says_nothing_new` missed both: it
compared against the *display name* (`components/ChatWindow`), which a leaf file
name never equals, and it did not strip the extension. Now compared against the
district's last path component with the extension removed, and universal entry
points (`index`, `main`, `mod`, `lib`, `app`, `__init__`, …) are refused
outright. The monument itself is untouched — PRD §8 still labels that building;
it is only a useless label for the *district*.

**E. A file speaks for the directory it is in, and no further.** The
doc-comment extractor fell back to the district's monument wherever it sat in
the subtree. That described `qurio-toolset`'s **root** as "a DEV-ONLY Vite
plugin" (from `vite-plugins/manualReloadPlugin.js`) and `components/MediaWindow`
as a YouTube renderer (from `MediaWindow/renderers/YouTubeRenderer.jsx`). An
`ANCHOR_NAMES` file is a directory's declared front door and may speak for it;
any other file speaks by proximity, and proximity ends at the first
subdirectory.

**F. Three tool-output directories added to the industrial list.**
`.astro` (Astro's generated cache — `stickingplace/.astro` was reading as one of
that repository's source districts), `.playwright-mcp` / `playwright-report` /
`test-results` (browser-automation dumps — 106 files in `biwt`, 27 in
`Squigglo`, 8 in `qurio-toolset`, all reading as `config`), plus `.docusaurus`,
`.vercel`, `.netlify`, `.wrangler` for the same reason.

Net effect on the operator's repositories: five wrong or absurd labels removed,
three restatements removed, 152 tool-written files moved out of the civic
budget, and one new genuinely good description surfaced
(`stickingplace/pages/alexander-mackendrick`). Prose count went 44 → 38; the
informative count went 10 → 10. **That is the point: the losses were all
wrong.**

---

## 7. Sample tables — read these and decide

### `qurio-toolset` — 90 459 files, 1 571 civic, 55 neighborhoods

| neighborhood | kind | files | description | source |
|---|---|---:|---|---|
| `node_modules` | vendored | 61 458 | | — |
| `functions/node_modules` | vendored | 13 848 | | — |
| `electron/node_modules` | vendored | 13 377 | | — |
| `src/services` | source | 223 | Centralized Window Management Service | doc ⚠ *one of 223 files* |
| `dist` | vendored | 191 | | — |
| `tests/services` | test | 172 | | — |
| `src/components` | source | 146 | most imported: BaseWindow.jsx | *inventory* |
| `docs` | docs | 88 | | — |
| `components/custom` | source | 75 | most imported: Icon.jsx | *inventory* |
| `/` | config | 69 | | — |
| `src/utils` | source | 58 | most imported: inkGeometry.js | *inventory* |
| `src/hooks` | source | 46 | useDropZone Hook | doc ⚠ *one of 46 files* |
| `scripts` | source | 39 | most imported: perf-trace.mjs | *inventory* |
| `tests/utils` | test | 36 | | — |
| `services/tools` | source | 32 | Tool Registry | doc ✅ |
| `functions` | source | 31 | Cloud Functions for Qurio Gmail Integration | manifest ✅ |
| `tests` | test | 31 | | — |
| `components/ui` | source | 28 | most imported: button.jsx | *inventory* |
| `src` | source | 26 | most imported: utils.js | *inventory* |
| `tests/components` | test | 24 | | — |
| `components/BrowserWindow` | source | 23 | most imported: internalPages.js | *inventory* |
| `.claude/skills` | docs | 22 | 16 files named skill | *inventory* |
| `components/MediaWindow` | source | 21 | most imported: YouTubeRenderer.jsx | *inventory* |
| `src/services/userModel` | source | 20 | threadIntegrationService — the L2 tier of the user-model pipeline. | doc ⚠ |
| `electron` | source | 18 | Qurio desktop app with native browser window overlays | manifest ✅ |
| `services/integrations` | source | 17 | Gmail Service — Frontend integration with Firebase Functions | doc ✅ |
| `services/settings` | source | 16 | The settings entry contract. | doc ✗ *restates* |
| `services/memory` | source | 15 | Memory System — Main Export | doc ✗ *restates* |
| `components/a2ui` | source | 13 | A2UI Renderer | doc ✗ *restates* |
| `services/dropZoneService` | source | 13 | Drop Zone Service | doc ✗ *restates* |
| `signing/apple` | config | 13 | | — |
| `src/config` | source | 12 | Firebase Configuration and Initialization | doc ✅ |
| `components/ChatWindow` | source | 11 | | — |
| `components/InkLayer` | source | 11 | | — |
| `services/adapters` | source | 8 | Tool Format Converters | doc ✅ |
| `components/GmailWindow` | source | 7 | ContactAutocomplete — the Gmail composer's To/Cc/Bcc field. | doc ⚠ |
| `.playwright-mcp` | vendored | 8 | | — |
| `styles/generated` | vendored | 4 | | — |

Kinds: source 1 025 · test 320 · docs 118 · config 45 · assets 33 · unknown 21 ·
build 3 · data 6 · vendored 88 888.

### `stickingplacebooks` — 81 066 files, 1 002 civic, 31 neighborhoods

| neighborhood | kind | files | description | source |
|---|---|---:|---|---|
| `node_modules` | vendored | 65 955 | | — |
| `functions/node_modules` | vendored | 12 571 | | — |
| `.next` | vendored | 1 538 | | — |
| `media` | assets | 172 | | — |
| `books` | assets | 153 | 67 files named cover | *inventory* |
| `src/app` | source | 77 | most imported: BookContext.tsx | *inventory* |
| `src/components` | source | 68 | most imported: PdfViewerDialog.tsx | *inventory* |
| `functions/lib` | source | 54 | most imported: types.js · 50 % source, 50 % vendored | *inventory* ⚠ *tsc output* |
| `/` | config | 42 | | — *(was the create-next-app line)* |
| `components/analytics` | source | 42 | most imported: BookAnalyticsView.tsx | *inventory* |
| `emulator-seed-2026-03-29-154659` | unknown | 39 | | — |
| `components/ui` | source | 38 | most imported: button.tsx | *inventory* |
| `lib/firestore` | source | 34 | most imported: books.ts | *inventory* |
| `src/types` | source | 32 | | — |
| `components/admin` | source | 30 | most imported: BookEditorLayout.tsx | *inventory* |
| `docs` | docs | 29 | | — |
| `functions/src` | source | 27 | Firebase Cloud Functions | doc ✗ |
| `components/press-kit` | source | 27 | most imported: styles.ts | *inventory* |
| `src/lib` | source | 25 | most imported: utils.ts | *inventory* |
| `public` | assets | 19 | | — |
| `scripts` | source | 18 | | — |
| `lib/data` | source | 17 | Data layer exports | doc ✗ |
| `releasenotes` | docs | 14 | | — |
| `lib/demo` | source | 11 | Demo mode utilities | doc ✗ |
| `scraper` | config | 8 | This standalone tool scrapes book data from the existing Sticking… | readme ✅ |
| `scraper/src` | source | 8 | Types for web scraper | doc ✗ |
| `components/dashboard` | source | 8 | | — |
| `src` | source | 5 | most imported: AuthContext.tsx | *inventory* |
| `functions` | config | 3 | | — |
| `emulator-data` | config | 1 | | — |
| `emulator-data/storage_export` | config | 1 | | — |

### `vc-tower` — 74 806 files, 13 299 "civic", 11 neighborhoods ← the broken one

| neighborhood | kind | files | description |
|---|---|---:|---|
| `web/node_modules` | vendored | 57 587 | |
| `vcsheet-scraper/partners` | **config** | 4 418 | ⚠ scraped `*.json` |
| `vcsheet-scraper/partners_html` | **source** | 4 418 | ⚠ scraped `*.html` |
| `web/.next` | vendored | 3 917 | |
| `vcsheet-scraper/funds` | **config** | 2 111 | ⚠ scraped `*.json` |
| `vcsheet-scraper/funds_html` | **source** | 2 111 | ⚠ scraped `*.html` |
| `web/src` | source | 100 | ⚠ the whole application, one blob |
| `/` | config | 83 | Investor enrichment orchestrator with a step-by-step debug UI. ✅ |
| `web` | config | 54 | *(was the create-next-app line)* |
| `vcsheet-scraper` | source | 4 | Standalone Python scraper for vcsheet.com. ✅ |
| `web/.playwright-mcp` | vendored | 3 | |

### `biwt` — 62 430 files, 223 civic, 20 neighborhoods, **zero prose**

| neighborhood | kind | files | description |
|---|---|---:|---|
| `node_modules` / `functions/node_modules` / `.next` | vendored | 62 101 | |
| `.playwright-mcp` | vendored | 106 | *(was config)* |
| `/` | config | 46 | |
| `components/admin` | source | 30 | most imported: AdminPlayerContext.tsx |
| `components/ui` | source | 19 | most imported: button.tsx |
| `lib/hooks` | source | 18 | most imported: useEffects.ts |
| `components/player` | source | 17 | most imported: MiniWaveform.tsx |
| `app/admin` | source | 15 | 12 files named page |
| `src/app` | source | 12 | 5 files named route |
| `lib/firestore` | source | 12 | most imported: effects.ts |
| `lib/utils` | source | 12 | most imported: effectNumber.ts |
| `components/landing` | source | 11 | most imported: LandingPrimitives.tsx |
| `src/lib` | source | 9 | most imported: utils.ts |
| `lib/types` | source | 7 | most imported: effect.ts |
| `functions` | config | 6 | |
| `components/public` | source | 6 | most imported: BackToPlayer.tsx |

A good map with no words on it. `biwt` is the clearest case for the LLM:
partition and colour are exactly right, and the description column is empty
because the repository contains no sentence to quote.

### `click` — 166 files, 10 neighborhoods, the best case

| neighborhood | kind | files | description | source |
|---|---|---:|---|---|
| `docs` | docs | 41 | Click is a Python package for creating beautiful command line… | readme |
| `examples` | docs | 26 | This folder contains various Click examples. | readme |
| `tests` | test | 22 | | — |
| `src/click` | source | 18 | Click is a simple Python module inspired by the stdlib optparse… | doc |
| `tests/test_utils` | test | 14 | | — |
| `/` | config | 11 | Click is a Python package for creating beautiful command line… | readme |
| `tests/typing` | test | 11 | | — |
| `.github` | build | 10 | | — |
| `examples/complex` | source | 7 | complex is an example of building very complex cli applications | readme |
| `examples/imagepipe` | assets | 6 | imagepipe is an example application that implements some commands | readme |

60 % prose. This is what the extractor achieves on a well-documented public
library, and it is why the approach looked better than it is: the operator's
repositories are not written like Click.

---

## 8. Recommendation

**Keep the partition and the kinds. They are a real improvement and they are
done.** Decisions 2 and 3 are delivered: hue now means something, districts are
at a readable granularity, and it holds up from a 17-file repository to a
7 000-file one. All of it inside PRD §10.3's channel-48 band, hue only.

**Decision 4 is not delivered, and I do not think more rules will deliver it.**
14 % of districts get prose; 6 % get prose worth reading. The three remaining
failure modes are all judgement calls that no path rule can make:

1. Deciding whether one file's doc comment fairly describes a 223-file
   directory. `WindowManager.js` genuinely does describe window management; the
   question is whether `src/services` is *about* that, and answering it requires
   reading the other 222 files.
2. Recognising that "Drop Zone Service" over a directory named
   `dropZoneService` is a tautology, when the words are not literally equal.
3. Saying anything at all about `biwt`, `Squigglo` or `stickingplace`, which
   between them have 60 districts and two sentences of prose in the whole
   corpus.

An LLM does all three trivially, and the input it needs is already computed —
each district's file list, kinds mix, monument and import edges. **A local model
would keep PRD §2 intact**; a remote one would not, and PRD §2 forbids it
regardless of convenience. Cost is bounded: 55 districts for the largest
repository, cached on the same content digest the description cache already
uses, regenerated only when a district's contents change.

**Concrete next steps, in the order I would do them:**

1. **Fix the sizing budget's blind spot** (the vc-tower failure). Generalise
   ADR-0086's "vendored trees are held out of the budget" from *directory names*
   to *behaviour*: a flat directory holding thousands of files that all share
   one extension is a mass whatever it is called, and should be one district
   held out of the budget — exactly what PRD §8 says about industrial zones,
   applied to a tree that simply is not called `node_modules`. This is the one
   change that would fix a real repository's map today.
2. **Then reconsider the LLM for descriptions.** Keep the extractor: a real
   README or manifest description should always win over a generated one, and
   the sanitiser must stay in front of whatever a model returns. Use the model
   only where the extractor now returns `None` or a restatement — 141 of 179
   districts.
3. Ship the inventory line to the drill-down panel rather than the map, since
   the monument is already labelled there (PRD §8).
4. Guard the serialised `DescribeStats` counters before anything snapshots a
   `Neighborhoods`.

The honest summary for the operator: **you got the coloured, well-cut city you
asked for, and you did not get the captions.**
