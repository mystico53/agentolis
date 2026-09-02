# `tests/corpora/` — real repositories, recorded

These are the M1 gate's **real** inputs (PRD §16). Each `.corpus` file is a
complete record of one repository as far as `polis-layout` is concerned: for
every file, its logical path, its size in bytes, its landmark class, its position
in the growth sequence, and the commit times of its first addition and its last
touch. Nothing else about a repository reaches the layout, so nothing else is
recorded — see `polis_repo::manifest` for the argument and the format.

They exist because the M1 acceptance table used to be asserted against
`polis_repo::synthetic` alone, with a four-assertion stub standing in for "and it
works on a real repository". A corpus written to exercise the layout is chosen by
the same people who tune the layout, and it will keep agreeing with them. These
do not.

## What is here, and what each one is for

| file | repository | files | commits | span | why it is here |
|---|---|---:|---:|---:|---|
| `click.corpus` | [pallets/click](https://github.com/pallets/click) | 166 | 3 333 | 12 years | The **small repository** — the first city a new user sees, and the size the layout was quietly failing at. Big leaf directories, a real old town. |
| `pytest.corpus` | [pytest-dev/pytest](https://github.com/pytest-dev/pytest) | 690 | 17 715 | 18 years | A history that **accelerated**: 0.1 % of its files date from its first year, which is the case `polis_layout::age`'s equalisation exists for. |
| `polis-day-one.corpus` | this repository | 107 | 9 | **1 day** | A repository younger than a day. Nothing in a fixture library is, and it is the other common case: a project started this morning. It failed three acceptance criteria before ADR-0078. |

Pinned commits:

```
click           36baa15ff831b939a22bc527cd76ce653ef6f66d
pytest          51e9a9f148cd2509a31e3fa0d2b1b3204c2b0dd7
polis-day-one   1375e2aec38d85d11a5f994096292caaf55e9556
```

## Why a manifest and not a vendored checkout

A checked-in copy of `click` would be a megabyte of Python that the layout never
reads, another project's licence in this tree, and a `.git` directory that would
still have to be vendored for the growth order. The manifest is 10 KB of tab
separated integers and paths, it is diffable, and it is *exactly* the input.

What it deliberately does **not** carry is file content, so a corpus fixture
exercises the city and not PRD §9's streets — those need `tree-sitter` over real
source, and the two pinned `hamlet`/`town` fixtures cover them.

## Regenerating one

There is no network access anywhere in the test suite. Capturing is a developer
action; CI reads these files.

```sh
git clone https://github.com/pallets/click.git /tmp/click
git -C /tmp/click checkout 36baa15ff831b939a22bc527cd76ce653ef6f66d

POLIS_CAPTURE_ROOT=/tmp/click \
POLIS_CAPTURE_NAME=click \
POLIS_CAPTURE_ORIGIN=https://github.com/pallets/click.git \
  cargo test -p polis-repo --test corpus_capture -- --ignored --nocapture
```

`POLIS_CAPTURE_SKIP_MASSED=1` skips PRD §8's massed trees, which is what
`polis-day-one` was captured with (this repository has a `target/`).

**If you re-pin a commit, update the table above in the same change.** A fixture
whose provenance is not written down is a fixture nobody can check, and every
number the gate reports is a number about it.

## The measurements these produced

With the code at the commit that added them:

| corpus | files | solidity | straight border | rulers | longest stroke | 4-and-5+ | coverage | p95:p05 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `click` | 166 | 0.680 | 14.2 % | 0 | 66.3 % | 53.3 % | 19.9 % | 56.0× |
| `pytest` | 690 | 0.831 | 13.0 % | 0 | 62.1 % | 66.8 % | 27.9 % | 7.0× |
| `polis-day-one` | 107 | 0.877 | 14.4 % | 0 | 52.7 % | 58.9 % | 25.2 % | 24.6× |
| `synthetic::repository(5 000)` | 4 965 | 0.793 | 7.6 % | 0 | 37.1 % | 64.9 % | 31.1 % | 9.4× |

The spread across the first three columns is the point. It is a spread across
*repositories*, produced by one generator, and it is what a single tuned fixture
cannot show.
