# `docs/design` — the bake-off, and where its code went

This directory holds the design record: the three layout architectures that were
prototyped and scored before `polis-layout` existed, the reviews that judged
them, and the images they were judged on. The verdict and its reasoning are in
[`../DECISIONS.md`](../DECISIONS.md) — ADR-0052 onward.

**The prototype source is no longer here.** Each of the three carried a
standalone Rust program — 13 761 lines across `accretion/prototype`,
`treemap-arterials/prototype` and `voronoi-organic/prototype-src`. None of them
is a workspace member, none has ever been built by CI, and none can be: every
one of them was superseded by `polis-layout`, which is where the surviving ideas
live and where they are tested. What they still did was distort every line count
taken of this repository by about 8 %, and read to anyone new as code that runs.

They are archived at the `archive/design-prototypes` tag, which is the last
commit that contained them:

```sh
git show archive/design-prototypes                       # the note, and why
git checkout archive/design-prototypes -- docs/design    # bring them all back
```

Everything the ADRs actually cite — the `DESIGN.md` files, the rendered PNGs,
the layout dumps, the review notes — is still in this directory and always will
be. It is the argument that is load-bearing, not the abandoned implementation of
it (ADR-0110).
