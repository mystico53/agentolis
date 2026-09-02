#!/usr/bin/env bash
#
# Builds the fixture repositories PRD §16's golden-layout test runs against.
#
#   usage: tests/make-fixtures.sh <output-dir>
#
# > Fixture repos with pinned git history; snapshot the serialized `CityLayout`.
# > Run on two OSes in CI. This is the most important test in the suite —
# > everything else in the product depends on the map not moving. (PRD §16)
#
# ---------------------------------------------------------------------------
# Why every line of the git setup below is load-bearing
# ---------------------------------------------------------------------------
#
# The layout is a function of the repository, so a fixture that is not itself
# byte-reproducible turns the determinism gate into a test of the clock. Six
# things have to be pinned, and five of them are things git takes from the
# environment unless told otherwise:
#
#   1. AUTHOR and COMMITTER identity      -> GIT_AUTHOR_*, GIT_COMMITTER_*
#   2. AUTHOR and COMMITTER timestamps    -> GIT_AUTHOR_DATE, GIT_COMMITTER_DATE
#      This is the one that matters most for Polis specifically: PRD §7.1 makes
#      commit order the growth order, and `FileMeta::added_at` / `last_touched`
#      are commit times. A fixture built with `git commit` and no date is a
#      different city every second.
#   3. The user's global and system config -> GIT_CONFIG_GLOBAL/SYSTEM=/dev/null
#      A developer with `commit.gpgsign=true`, a `core.autocrlf`, a commit
#      template or a `init.defaultBranch` produces different objects.
#   4. Line endings                        -> core.autocrlf=false, core.eol=lf,
#      plus a `.gitattributes` of `* -text`. A CRLF checkout changes every file
#      size, and file size is PRD §7.3's footprint area.
#   5. The executable bit                  -> core.fileMode=false, so a umask
#      difference cannot change a tree object.
#   6. The default branch name             -> `git init -b main`.
#
# With all six pinned the commit SHAs themselves are reproducible, which is why
# `m1_gate.rs` asserts them literally: if this script drifts, the gate says so
# in the fixture test rather than in the golden file, where it would look like a
# layout regression.
#
# ---------------------------------------------------------------------------
# What is in the fixtures
# ---------------------------------------------------------------------------
#
#   hamlet/ — three files, two commits. The smallest thing that produces a city
#             at all. A readable golden file when something breaks.
#
#   town/   — the non-trivial one, and every part of it is there to exercise
#             something the layout has to get right:
#               * nested directories four deep, so district hosting has a chain
#                 to walk (`lots::plan` step 3);
#               * non-ASCII filenames in NFC — `café`, `módulo`, `日本語`,
#                 `naïve` — because `LogicalPath` folds ASCII case only
#                 (ADR-0028) and a byte-slicing bug there is a panic, not a
#                 wrong answer (ADR-0046);
#               * a DELETED file, so the growth sequence contains a path the
#                 working tree does not (PRD §7.5's vacant lot);
#               * a RENAMED file, which `polis-repo` pins `--no-renames` for and
#                 therefore sees as a delete plus an add — two growth entries,
#                 which is exactly what the layout must be stable under;
#               * a `vendor/` tree, so PRD §8's industrial mass has something to
#                 be;
#               * `src/main.rs` and `src/lib.rs`, so PRD §8's monuments have
#                 anchors to rank;
#               * files of visibly different sizes, because PRD §7.3's footprint
#                 is proportional to sqrt(bytes) and a fixture of equal-sized
#                 files would not notice if that stopped working;
#               * REAL CROSS-DISTRICT IMPORTS (commit 9), so PRD §9's `streets`
#                 array is non-empty in a pinned snapshot. Before those existed
#                 all three golden files pinned `streets: []`, which is why a
#                 release-only reordering of that field passed 57 determinism
#                 tests. See `write_importing`.
#
# ---------------------------------------------------------------------------
# Known limitation
# ---------------------------------------------------------------------------
#
# The non-ASCII names are written in NFC. macOS normalises filenames to NFD, so
# on APFS the walk would produce different logical paths and a different city —
# ADR-0028 records that Polis does no Unicode normalisation, and this fixture
# would be the first place a macOS CI leg noticed. Windows and Linux, which is
# what PRD §16's two OSes are, both preserve the bytes.

set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <output-dir>" >&2
    exit 2
fi

OUT="$1"

# Bumped whenever the content or the history below changes. `m1_gate.rs` reads
# it back out of the stamp file and rebuilds when it does not match, so a stale
# fixture in a warm CI cache cannot silently pass yesterday's golden file.
FIXTURE_VERSION=2

STAMP="$OUT/.polis-fixture-version"
if [ -f "$STAMP" ] && [ "$(cat "$STAMP")" = "$FIXTURE_VERSION" ]; then
    exit 0
fi

rm -rf "$OUT"
mkdir -p "$OUT"

# Nothing the developer has configured may reach these repositories.
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_CONFIG_SYSTEM=/dev/null
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE 2>/dev/null || true

export GIT_AUTHOR_NAME="Polis Fixture"
export GIT_AUTHOR_EMAIL="fixture@polis.invalid"
export GIT_COMMITTER_NAME="Polis Fixture"
export GIT_COMMITTER_EMAIL="fixture@polis.invalid"

REPO=""

# init <name>
init() {
    REPO="$OUT/$1"
    mkdir -p "$REPO"
    git -C "$REPO" init -q -b main
    git -C "$REPO" config core.autocrlf false
    git -C "$REPO" config core.eol lf
    git -C "$REPO" config core.fileMode false
    git -C "$REPO" config core.quotePath false
    git -C "$REPO" config commit.gpgsign false
    git -C "$REPO" config user.name "$GIT_AUTHOR_NAME"
    git -C "$REPO" config user.email "$GIT_AUTHOR_EMAIL"
    printf '* -text\n' > "$REPO/.gitattributes"
}

# write <relative-path> <line-count>
#
# Content is derived from the path and the line count, so it is identical on
# every machine and the file size is a pinned function of both.
write() {
    local rel="$1" lines="$2" i
    mkdir -p "$REPO/$(dirname "$rel")"
    {
        printf '// %s\n' "$rel"
        i=0
        while [ "$i" -lt "$lines" ]; do
            printf 'line %04d of %s\n' "$i" "$rel"
            i=$((i + 1))
        done
    } > "$REPO/$rel"
}

# write_importing <relative-path> <line-count> <import-line>...
#
# `write`, but the file opens with real import statements, so
# `polis_repo::imports` extracts a genuine cross-district edge from it.
#
# This exists because of a specific bug. `city::build_streets` asserted its
# input was in canonical order with a `debug_assert` and returned it *unsorted*,
# so `CityLayout::streets` serialized in caller order in release and in layout
# order in debug. Every `cargo test` passed. The reason no golden file noticed
# is that the fixtures had no cross-district import at all, so all three pinned
# `streets: []` — a field that is always empty cannot regress visibly. PRD §16
# calls the golden test the most important in the suite; it only covers what the
# fixtures actually produce.
write_importing() {
    local rel="$1" lines="$2" i line
    shift 2
    mkdir -p "$REPO/$(dirname "$rel")"
    {
        printf '// %s\n' "$rel"
        for line in "$@"; do
            printf '%s\n' "$line"
        done
        i=0
        while [ "$i" -lt "$lines" ]; do
            printf 'line %04d of %s\n' "$i" "$rel"
            i=$((i + 1))
        done
    } > "$REPO/$rel"
}

# commit <iso-8601-utc> <message>
commit() {
    local when="$1" message="$2"
    GIT_AUTHOR_DATE="$when" GIT_COMMITTER_DATE="$when" \
        git -C "$REPO" commit -q -m "$message"
}

# ---------------------------------------------------------------------------
# hamlet — the smallest city that exists
# ---------------------------------------------------------------------------

init hamlet
write README.md 12
write src/main.rs 40
git -C "$REPO" add -A
commit "2021-01-04T09:00:00+00:00" "the first commit"

write src/util.rs 25
git -C "$REPO" add -A
commit "2021-02-11T14:30:00+00:00" "a helper"

# ---------------------------------------------------------------------------
# town — nested, non-ASCII, a deletion, a rename, an industrial tree
# ---------------------------------------------------------------------------

init town

# Commit 1 — the old town. PRD §7.1: these end up in the middle of the map.
write README.md 30
write src/main.rs 120
write src/lib.rs 200
git -C "$REPO" add -A
commit "2020-03-02T08:15:00+00:00" "the founding"

# Commit 2 — the first districts.
write src/auth/session.rs 180
write src/auth/tokens.rs 90
write src/net/client.rs 240
write src/net/server.rs 310
write src/util.rs 60
git -C "$REPO" add -A
commit "2020-05-19T11:00:00+00:00" "auth and net"

# Commit 3 — non-ASCII, in NFC. See the header.
write "src/café/módulo.rs" 75
write "src/café/naïve.rs" 45
write "docs/日本語.md" 20
write "docs/guide.md" 130
git -C "$REPO" add -A
commit "2020-08-07T16:45:00+00:00" "l10n and docs"

# Commit 4 — four levels deep, so district hosting has a chain to walk.
write src/auth/providers/oauth/google.rs 140
write src/auth/providers/oauth/github.rs 110
write src/auth/providers/ldap.rs 70
write tests/auth_test.rs 95
write tests/net_test.rs 85
git -C "$REPO" add -A
commit "2020-11-23T10:05:00+00:00" "providers and tests"

# Commit 5 — a file that will be deleted later, and an industrial tree.
write src/legacy/old_client.rs 260
write src/legacy/shim.rs 40
write vendor/leftpad/index.js 15
write vendor/leftpad/package.json 8
write vendor/bignum/bignum.js 900
git -C "$REPO" add -A
commit "2021-02-14T13:20:00+00:00" "legacy and vendor"

# Commit 6 — THE DELETION. PRD §7.5: the growth sequence keeps a path the
# working tree no longer has, and the layout leaves a vacant lot.
git -C "$REPO" rm -q -r src/legacy
commit "2021-04-30T09:40:00+00:00" "drop the legacy client"

# Commit 7 — THE RENAME. `polis-repo` pins `--no-renames`, so this is a delete
# plus an add and produces two growth entries.
mkdir -p "$REPO/src/utils"
git -C "$REPO" mv src/util.rs src/utils/helpers.rs
commit "2021-07-15T15:10:00+00:00" "move util into utils"

# Commit 8 — new files on the periphery. PRD §7.1: these ring the edge.
write src/ui/app.tsx 320
write src/ui/routes.ts 150
write src/ui/components/button.tsx 60
write src/ui/components/table.tsx 210
write examples/basic.rs 55
git -C "$REPO" add -A
commit "2021-10-01T12:00:00+00:00" "the ui district"

# Commit 9 — REAL CROSS-DISTRICT IMPORTS, so PRD §9's streets are non-empty in a
# pinned snapshot. `src/net` imports from `src/auth` and `src/utils`; `src/ui`
# imports twice from `src/ui/components`, so one street also carries an
# `edge_count` above one and the width PRD §9 draws is covered too.
write_importing src/net/gateway.rs 40 \
    'use crate::auth::tokens;' \
    'use crate::utils::helpers;'
write_importing src/ui/store.ts 35 \
    'import { Button } from "./components/button";' \
    'import { Table } from "./components/table";'
git -C "$REPO" add -A
commit "2021-11-18T09:25:00+00:00" "wire the gateway and the ui store"

printf '%s' "$FIXTURE_VERSION" > "$STAMP"
