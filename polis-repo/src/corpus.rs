//! The ubiquity discount — TF-IDF over your own corpus (PRD §6.1).
//!
//! > Every agent reads the README, `package.json`, and top-level config.
//! > Maintain a rolling count over the last N sessions of how many read each
//! > path, and scale each observation by `log(N / sessions_that_read_path)`. A
//! > path read by 90% of sessions contributes ~nothing; one read by 3%
//! > dominates.
//!
//! Persisted in SQLite at `$XDG_STATE_HOME/polis/corpus.db`
//! (`%LOCALAPPDATA%\polis\corpus.db` on Windows). This is the **only** thing
//! Polis writes to disk, and it must contain no file contents and no
//! personally-identifying attribute — only paths and counts (ADR-0005).
//!
//! Cold start with no corpus falls back to a shipped denylist of common
//! orientation files ([`Denylist`], [`COLD_START_DENYLIST`]).
//!
//! # The three regimes, and when each one applies
//!
//! [`Corpus::regime`] is the whole story, and it is worth stating plainly because
//! day one is the case that actually matters:
//!
//! | Sessions in the store | [`Regime`] | [`Corpus::ubiquity_discount`] returns |
//! |---|---|---|
//! | store unavailable | [`Regime::Degraded`] | the denylist verdict, forever |
//! | `0 ..< `[`COLD_START_SESSIONS`] | [`Regime::ColdStart`] | the denylist verdict |
//! | `>= `[`COLD_START_SESSIONS`] | [`Regime::Learned`] | measured `idf(N, df)` |
//!
//! The switch happens once, at the twentieth recorded session, and it is a hard
//! switch rather than a blend. Two reasons. A blend needs the learned value to
//! be meaningful *before* the switch, and with `N < 20` a single session moves
//! `df/N` by five percentage points, so the "learned" half of the blend is noise
//! that would visibly wobble every territory. And a hard switch is testable —
//! [`Regime`] is a function of one integer, which a golden test can pin.
//!
//! The step at the switch is deliberately small in the direction that matters.
//! [`DENYLIST_DISCOUNT`] is `0.05`, which is what [`idf`] returns for a path read
//! by roughly 90% of sessions, so a denylisted path barely moves when the corpus
//! takes over. What *does* move is the other half: a path this operator's agents
//! read constantly that no shipped list could have known about drops from `1.0`
//! to near zero, and a file the denylist wrongly suppressed climbs back to `1.0`.
//! That asymmetry is the entire value of learning the corpus, and it is why the
//! denylist stops applying at the switch rather than staying on as a floor.
//!
//! # Weight range: normalised IDF, not raw `ln(N/df)`
//!
//! [`idf`] returns `ln(N/df) / ln(N)`, which is the PRD's formula divided by the
//! constant `ln(N)`. Every *ratio* between two paths is preserved exactly — which
//! is all PRD §6.2 uses, since its convergence test is a fraction of total weight
//! — but the range becomes `(0, 1]` instead of `(0, ln N]`.
//!
//! That bound is load-bearing, not tidiness. ADR-0020 fixed the territory
//! iso-thresholds against "one full-weight kernel == 1.0" and explicitly refused
//! to normalise against the observed field maximum. Raw IDF at `N = 1000` scales a
//! lone observation by `6.9`, which is larger than the `5.0` PRD §6.1 gives a
//! `Glob` and would push any rare-path territory straight into the core band. A
//! *discount* must never amplify: `1.0` means "no discount", and that is also
//! what an empty corpus returns.
//!
//! # `rusqlite` is bundled, and single-threaded
//!
//! The `bundled` feature compiles SQLite in, so a fresh clone needs no system
//! library. A `Connection` is **not** `Sync`: own it on one thread — the world
//! thread, which is already the single writer (PRD §5) — rather than wrapping it
//! in a mutex and inviting a lock on the render path.
//!
//! # A broken store is never fatal
//!
//! The corpus is a quality signal, not critical state. Every failure mode —
//! a missing directory, a corrupt file, a database locked by another Polis, a
//! schema written by a newer build — resolves to [`Corpus::open_or_degraded`]
//! returning a working [`Corpus`] that answers from the denylist. Nothing in this
//! module can prevent Polis from starting.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use polis_events::{LogicalPath, SessionId};
use rusqlite::{Connection, OptionalExtension};

/// Sessions below which the corpus is not yet trusted and
/// [`COLD_START_DENYLIST`] is used instead.
pub const COLD_START_SESSIONS: u32 = 20;

/// The rolling window, in sessions, that [`Corpus::prune`] keeps.
pub const ROLLING_WINDOW_SESSIONS: u32 = 500;

/// The discount applied to a [`Denylist`] hit during [`Regime::ColdStart`].
///
/// Chosen to equal [`idf`] for a path read by ~90% of sessions — the PRD's own
/// example of a path that "contributes ~nothing" — so the cold-start regime
/// agrees with the learned regime about the files both of them recognise, and
/// the switch at [`COLD_START_SESSIONS`] is not a visible jolt.
pub const DENYLIST_DISCOUNT: f32 = 0.05;

/// The floor [`idf`] clamps to.
///
/// A path read by *every* session has a true IDF of exactly zero, and zero would
/// delete the observation rather than discount it — an agent that only ever
/// touched ubiquitous files would have no territory at all and PRD §6.2's
/// "fraction of total weight" test would divide by zero. A hundredth of a
/// full-weight observation is indistinguishable from nothing on screen and is
/// still a number.
pub const MIN_DISCOUNT: f32 = 0.01;

/// The schema version this build writes and understands.
///
/// A store carrying a *higher* version was written by a newer Polis; it is left
/// untouched and this build degrades (see [`Corpus::open_or_degraded`]). A lower
/// version is migrated forward in place by `migrate`.
pub const SCHEMA_VERSION: u32 = 1;

/// How long a write waits for another Polis instance before giving up.
///
/// Short on purpose: the corpus is written from the world thread (PRD §5), and a
/// multi-second stall there is a dropped frame. A lost write costs one session of
/// counts out of [`ROLLING_WINDOW_SESSIONS`].
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// The rolling per-path session-read counts.
///
/// Open one per process with [`Corpus::open_or_degraded`], record one call to
/// [`Corpus::record_session`] per finished session, and ask
/// [`Corpus::ubiquity_discount`] for the multiplier on every observation.
#[derive(Debug)]
pub struct Corpus {
    store: Store,
    /// Sessions currently inside the rolling window. Mirrors `COUNT(*)` on the
    /// session table and is maintained in step with it.
    sessions: u32,
    /// Document frequency: how many sessions read each path. `BTreeMap`, never
    /// `ahash` — this map is iterated by [`Corpus::document_frequencies`] and
    /// `ahash`'s `RandomState` is seeded per process (PRD §7.4).
    df: BTreeMap<LogicalPath, u32>,
    denylist: Denylist,
    window: u32,
}

/// Where the counts live, when they live anywhere.
#[derive(Debug)]
enum Store {
    /// A usable SQLite connection, on disk or `:memory:`.
    Live(Connection),
    /// No store. Reads answer from the denylist; writes are dropped.
    Degraded(String),
}

/// Which rule [`Corpus::ubiquity_discount`] is currently applying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Regime {
    /// Fewer than [`COLD_START_SESSIONS`] sessions recorded: the shipped
    /// [`Denylist`] answers.
    ColdStart,
    /// The corpus speaks for itself: `idf(N, df)`.
    Learned,
    /// No store could be opened. Behaves like [`Regime::ColdStart`] and never
    /// leaves it, because nothing is being recorded.
    Degraded,
}

impl Regime {
    /// True when the answer comes from the shipped denylist rather than from
    /// measured counts.
    pub fn is_cold(self) -> bool {
        matches!(self, Self::ColdStart | Self::Degraded)
    }
}

/// Why a store could not be opened, classified finely enough to decide whether
/// the file on disk is salvageable.
#[derive(Debug)]
enum OpenFailure {
    /// SQLite refused. Carries the error so the corruption test can inspect it.
    Sqlite(rusqlite::Error),
    /// The parent directory could not be created.
    Io(std::io::Error),
    /// The store was written by a newer Polis. Never destroyed, never migrated
    /// backwards.
    FutureSchema(u32),
}

impl std::fmt::Display for OpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "sqlite: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::FutureSchema(v) => write!(
                f,
                "corpus.db is schema v{v}, this build understands v{SCHEMA_VERSION}"
            ),
        }
    }
}

impl OpenFailure {
    /// True when the file itself is unusable and re-creating it is the only way
    /// forward.
    ///
    /// **Deliberately false for a busy or locked database.** Another Polis
    /// holding a write lock is a completely normal condition (PRD §2 is
    /// single-operator, not single-process), and deleting a healthy store
    /// because a sibling had it open would throw away hundreds of sessions of
    /// counts to fix a condition that resolves itself in milliseconds.
    fn is_corruption(&self) -> bool {
        match self {
            Self::Sqlite(rusqlite::Error::SqliteFailure(e, _)) => matches!(
                e.code,
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
            ),
            Self::Sqlite(_) | Self::Io(_) | Self::FutureSchema(_) => false,
        }
    }
}

impl Corpus {
    /// Opens or creates the store, applying any pending schema migration.
    ///
    /// Prefer [`Corpus::open_or_degraded`] on the startup path: this returns the
    /// error, and PRD §6.1's store must never be a reason not to start.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_raw(path).map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Opens the store, and **cannot fail**.
    ///
    /// The recovery ladder, in order:
    ///
    /// 1. Open, migrate, load. Normal.
    /// 2. If that failed because the file is not a database or is corrupt, move
    ///    it aside to `<path>.corrupt` and try once more with a fresh file. The
    ///    counts are reconstructible by using Polis for a week; a permanently
    ///    broken store is not.
    /// 3. Otherwise — locked by another instance, unwritable directory, a schema
    ///    from a newer build — return a [`Regime::Degraded`] corpus that answers
    ///    from the denylist and records nothing. **The file is left exactly as it
    ///    was.**
    pub fn open_or_degraded(path: &Path) -> Self {
        match Self::open_raw(path) {
            Ok(corpus) => corpus,
            Err(first) => {
                if first.is_corruption() {
                    tracing::warn!(
                        path = %path.display(),
                        error = %first,
                        "corpus.db is corrupt; moving it aside and starting a fresh one"
                    );
                    if move_aside(path).is_ok() {
                        match Self::open_raw(path) {
                            Ok(corpus) => return corpus,
                            Err(second) => {
                                return Self::degraded(format!(
                                    "corpus.db was corrupt and could not be recreated: {second}"
                                ))
                            }
                        }
                    }
                }
                tracing::warn!(
                    path = %path.display(),
                    error = %first,
                    "corpus.db unavailable; falling back to the cold-start denylist"
                );
                Self::degraded(first.to_string())
            }
        }
    }

    /// Opens an in-memory store. Tests and `polis snapshot`, which must not
    /// touch the operator's real corpus.
    pub fn in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        configure(&conn, false);
        migrate(&conn)?;
        Self::loaded(conn).map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// A corpus with no store at all: every answer comes from the denylist and
    /// every write is dropped.
    ///
    /// Public because a caller that has decided not to persist anything at all
    /// (`polis replay`, a `--no-corpus` flag) wants the same behaviour a broken
    /// store gets, by the same code path.
    pub fn degraded(reason: impl Into<String>) -> Self {
        Self {
            store: Store::Degraded(reason.into()),
            sessions: 0,
            df: BTreeMap::new(),
            denylist: Denylist::default(),
            window: ROLLING_WINDOW_SESSIONS,
        }
    }

    /// The default on-disk location for this platform.
    ///
    /// `%LOCALAPPDATA%\polis\corpus.db` on Windows, `$XDG_STATE_HOME/polis/corpus.db`
    /// (falling back to `~/.local/state`) elsewhere.
    pub fn default_path() -> Option<PathBuf> {
        Some(Self::default_state_dir()?.join("corpus.db"))
    }

    /// The platform state directory, `polis` component included.
    ///
    /// The same resolution `docs/verified/hook-ipc.md` §5 settled for the hook
    /// endpoint file, with one deliberate difference: `XDG_RUNTIME_DIR` is not
    /// consulted. That directory is for *runtime* state and is documented to be
    /// deleted when the user's last session ends, which is correct for an
    /// endpoint file naming a live port and wrong for a rolling count that only
    /// becomes useful after twenty sessions. The Windows answer is identical
    /// (`%LOCALAPPDATA%`) precisely so the two files sit together.
    ///
    /// `None` means no environment variable identified a home; the caller should
    /// use [`Corpus::degraded`], not invent a path.
    pub fn default_state_dir() -> Option<PathBuf> {
        state_dir()
    }

    /// Records that one session read a set of paths.
    ///
    /// Called once per session, at its end — **not per read**, which would make
    /// ubiquity a function of how chatty an agent is rather than how common the
    /// file is. Duplicate paths within one session count once, and a session
    /// recorded twice (a resumed session, a replay of the same recording) adds
    /// only the paths it had not contributed before: the unique key is
    /// `(session, path)`.
    ///
    /// Prunes to [`Corpus::window`] afterwards, so the store cannot grow without
    /// bound whatever the caller does.
    ///
    /// A [`Regime::Degraded`] corpus drops the write and returns `Ok`: the
    /// caller is a session-end handler and has nothing useful to do with the
    /// error.
    pub fn record_session(
        &mut self,
        session: &SessionId,
        paths: &[LogicalPath],
    ) -> anyhow::Result<()> {
        let Store::Live(conn) = &mut self.store else {
            return Ok(());
        };
        let tx = conn.transaction()?;
        let is_new_session = tx.execute(
            "INSERT OR IGNORE INTO session (session_id) VALUES (?1)",
            [session.as_str()],
        )? == 1;
        let ordinal: i64 = tx.query_row(
            "SELECT ordinal FROM session WHERE session_id = ?1",
            [session.as_str()],
            |row| row.get(0),
        )?;

        // Collected first so the in-memory index is only touched once the
        // transaction has actually committed.
        let mut first_time = Vec::new();
        {
            let mut insert =
                tx.prepare("INSERT OR IGNORE INTO observation (ordinal, path) VALUES (?1, ?2)")?;
            for path in paths {
                let key = store_key(path);
                if insert.execute(rusqlite::params![ordinal, &key])? == 1 {
                    first_time.push(path.clone());
                }
            }
        }
        tx.commit()?;

        if is_new_session {
            self.sessions = self.sessions.saturating_add(1);
        }
        for path in first_time {
            *self.df.entry(path).or_insert(0) += 1;
        }

        if self.sessions > self.window {
            self.prune(self.window)?;
        }
        Ok(())
    }

    /// The `log(N / sessions_that_read_path)` multiplier, normalised to `(0, 1]`.
    ///
    /// Returns `1.0` — no discount — for a path the corpus has never seen and for
    /// an empty corpus, so a cold start treats every unrecognised path as
    /// equally informative rather than silently zeroing every weight. Below
    /// [`COLD_START_SESSIONS`] the answer comes from the [`Denylist`] instead;
    /// see [`Corpus::regime`].
    pub fn ubiquity_discount(&self, path: &LogicalPath) -> f32 {
        match self.regime() {
            Regime::ColdStart | Regime::Degraded => {
                if self.denylist.matches(path) {
                    DENYLIST_DISCOUNT
                } else {
                    1.0
                }
            }
            Regime::Learned => idf(self.sessions, self.sessions_that_read(path)),
        }
    }

    /// Which rule [`Corpus::ubiquity_discount`] is currently applying.
    pub fn regime(&self) -> Regime {
        match self.store {
            Store::Degraded(_) => Regime::Degraded,
            Store::Live(_) if self.sessions < COLD_START_SESSIONS => Regime::ColdStart,
            Store::Live(_) => Regime::Learned,
        }
    }

    /// How many sessions the corpus covers. Below [`COLD_START_SESSIONS`] the
    /// [`Denylist`] answers instead.
    pub fn session_count(&self) -> u32 {
        self.sessions
    }

    /// How many sessions read one path — the `df` in `log(N / df)`.
    pub fn sessions_that_read(&self, path: &LogicalPath) -> u32 {
        self.df.get(path).copied().unwrap_or(0)
    }

    /// How many distinct paths the corpus has ever seen inside the window.
    pub fn path_count(&self) -> usize {
        self.df.len()
    }

    /// Every `(path, df)` pair, in [`LogicalPath`] order.
    ///
    /// Ordered because a `HashMap` walk that reaches a golden file is exactly
    /// the failure PRD §7.4 forbids, and this is the natural thing for a
    /// diagnostic dump to iterate.
    pub fn document_frequencies(&self) -> impl Iterator<Item = (&LogicalPath, u32)> + '_ {
        self.df.iter().map(|(p, n)| (p, *n))
    }

    /// True when no store is backing this corpus.
    pub fn is_degraded(&self) -> bool {
        matches!(self.store, Store::Degraded(_))
    }

    /// Why the store is unavailable, for the status rail.
    pub fn degraded_reason(&self) -> Option<&str> {
        match &self.store {
            Store::Degraded(reason) => Some(reason),
            Store::Live(_) => None,
        }
    }

    /// The denylist consulted during [`Regime::ColdStart`].
    pub fn denylist(&self) -> &Denylist {
        &self.denylist
    }

    /// The denylist, mutably — the supported way to extend the shipped list with
    /// an operator's own orientation files.
    pub fn denylist_mut(&mut self) -> &mut Denylist {
        &mut self.denylist
    }

    /// Replaces the denylist wholesale.
    #[must_use]
    pub fn with_denylist(mut self, denylist: Denylist) -> Self {
        self.denylist = denylist;
        self
    }

    /// The rolling window, in sessions.
    pub fn window(&self) -> u32 {
        self.window
    }

    /// Sets the rolling window. Takes effect on the next
    /// [`Corpus::record_session`]; call [`Corpus::prune`] to apply it now.
    pub fn set_window(&mut self, sessions: u32) {
        self.window = sessions;
    }

    /// Drops sessions older than the rolling window, keeping the store bounded.
    ///
    /// PRD §6.1 says "a rolling count over the last N sessions"; without this the
    /// discount is a lifetime average and stops tracking how the repository is
    /// actually being worked on.
    ///
    /// "Older" is *insertion order*, never a timestamp: this crate reads no wall
    /// clock (PRD §7.4), and the session table's autoincrementing ordinal is
    /// already a total order that a clock change cannot perturb.
    ///
    /// Returns how many sessions were dropped.
    pub fn prune(&mut self, keep_sessions: u32) -> anyhow::Result<u32> {
        let Store::Live(conn) = &mut self.store else {
            return Ok(0);
        };
        let tx = conn.transaction()?;
        // The ordinal of the newest session that must go: skip `keep_sessions`
        // rows from the top and take the next one. `None` means there are not
        // that many sessions, so nothing is over the window.
        let cutoff: Option<i64> = tx
            .query_row(
                "SELECT ordinal FROM session ORDER BY ordinal DESC LIMIT 1 OFFSET ?1",
                [keep_sessions],
                |row| row.get(0),
            )
            .optional()?;
        let dropped = match cutoff {
            None => 0,
            Some(cutoff) => {
                tx.execute("DELETE FROM observation WHERE ordinal <= ?1", [cutoff])?;
                let n = tx.execute("DELETE FROM session WHERE ordinal <= ?1", [cutoff])?;
                u32::try_from(n).unwrap_or(u32::MAX)
            }
        };
        tx.commit()?;

        if dropped > 0 {
            // A prune changes the frequency of every surviving path, so the
            // index is rebuilt rather than patched. It happens once per window.
            let (sessions, df) = load_index(conn)?;
            self.sessions = sessions;
            self.df = df;
        }
        Ok(dropped)
    }

    /// Re-reads the counts another Polis instance may have written since this
    /// one opened the store.
    ///
    /// Two instances on one repository are a supported configuration (PRD §2 is
    /// single-*operator*), and SQLite makes the writes safe; it does not make
    /// this process's cached index notice them. Call this on a slow timer if the
    /// numbers matter, or never — a stale ubiquity discount is a slightly
    /// mis-weighted cloud, not an error.
    pub fn reload(&mut self) -> anyhow::Result<()> {
        if let Store::Live(conn) = &self.store {
            let (sessions, df) = load_index(conn)?;
            self.sessions = sessions;
            self.df = df;
        }
        Ok(())
    }

    // -- construction ------------------------------------------------------

    fn open_raw(path: &Path) -> Result<Self, OpenFailure> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(OpenFailure::Io)?;
            }
        }
        let conn = Connection::open(path).map_err(OpenFailure::Sqlite)?;
        configure(&conn, true);
        let found = schema_version(&conn).map_err(OpenFailure::Sqlite)?;
        if found > SCHEMA_VERSION {
            return Err(OpenFailure::FutureSchema(found));
        }
        migrate(&conn).map_err(OpenFailure::Sqlite)?;
        Self::loaded(conn)
    }

    fn loaded(conn: Connection) -> Result<Self, OpenFailure> {
        let (sessions, df) = load_index(&conn).map_err(OpenFailure::Sqlite)?;
        Ok(Self {
            store: Store::Live(conn),
            sessions,
            df,
            denylist: Denylist::default(),
            window: ROLLING_WINDOW_SESSIONS,
        })
    }
}

// ---------------------------------------------------------------------------
// The weighting itself
// ---------------------------------------------------------------------------

/// PRD §6.1's ubiquity discount, normalised to `(0, 1]`.
///
/// `ln(sessions / df) / ln(sessions)` — the PRD's `log(N / df)` divided by the
/// constant `ln(N)` so that the maximum is `1.0` rather than `ln(N)`. Ratios
/// between paths are unchanged, which is all PRD §6.2 consumes; see the module
/// documentation for why the bound matters to ADR-0020.
///
/// Total by construction. The degenerate cases, all of which occur in practice:
///
/// * `sessions <= 1` — nothing has been measured, so nothing is discounted:
///   `1.0`. (`ln(1)` is zero and the normalisation is undefined.)
/// * `df == 0` — a path the corpus has never seen. Maximally informative: `1.0`.
///   Falls out of clamping `df` into `1..=sessions` rather than being special-cased.
/// * `df >= sessions` — read by every session. Floored at [`MIN_DISCOUNT`] rather
///   than zero, so the observation is discounted into irrelevance instead of
///   deleted.
pub fn idf(sessions: u32, df: u32) -> f32 {
    if sessions <= 1 {
        return 1.0;
    }
    let n = f64::from(sessions);
    let df = f64::from(df.clamp(1, sessions));
    let value = (n / df).ln() / n.ln();
    // The computation is a ratio of two logs of small positive integers; the
    // result is in [0, 1] before clamping and cannot need more precision than
    // f32 carries. f32 is the weight type the whole layout uses.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "value is clamped into [MIN_DISCOUNT, 1.0]; f32 is the layout weight type"
    )]
    let out = value.clamp(f64::from(MIN_DISCOUNT), 1.0) as f32;
    out
}

// ---------------------------------------------------------------------------
// The cold-start denylist
// ---------------------------------------------------------------------------

/// Paths that are orientation reads in essentially every repository.
///
/// Used only until the corpus has enough sessions to speak for itself, and
/// deliberately generous: the cost of a false positive is one file's territory
/// contribution being under-weighted for twenty sessions, and the cost of a false
/// negative is every agent's cloud sitting on top of the same README.
///
/// Four pattern forms, all case-insensitive — see [`Denylist::push`]:
///
/// | Form | Matches |
/// |---|---|
/// | `README.md` | any file with that **name**, at any depth |
/// | `.github/` | anything **under** that directory |
/// | `*.lock` | any file with that **extension** |
/// | `docs/index.md` | that **exact** logical path |
///
/// Extend it with [`Denylist::push`] rather than by editing this list; the array
/// is the shipped default, not the only source.
pub const COLD_START_DENYLIST: &[&str] = &[
    // -- orientation prose ------------------------------------------------
    "README.md",
    "README",
    "README.rst",
    "README.txt",
    "CONTRIBUTING.md",
    "CHANGELOG.md",
    "CODE_OF_CONDUCT.md",
    "SECURITY.md",
    "LICENSE",
    "LICENSE.md",
    "LICENSE.txt",
    "COPYING",
    "NOTICE",
    // -- agent instructions: read at the start of literally every session --
    "CLAUDE.md",
    "AGENTS.md",
    ".cursorrules",
    ".windsurfrules",
    "GEMINI.md",
    // -- manifests and lockfiles ------------------------------------------
    "package.json",
    "Cargo.toml",
    "go.mod",
    "go.sum",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "Gemfile",
    "composer.json",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "*.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    // -- build entry points ------------------------------------------------
    "Makefile",
    "makefile",
    "CMakeLists.txt",
    "justfile",
    "Justfile",
    "Taskfile.yml",
    "Rakefile",
    // -- language and tooling config ---------------------------------------
    "tsconfig.json",
    "jsconfig.json",
    "rust-toolchain.toml",
    "rustfmt.toml",
    "clippy.toml",
    "vite.config.ts",
    "vite.config.js",
    "next.config.js",
    "next.config.mjs",
    "webpack.config.js",
    "rollup.config.js",
    "babel.config.js",
    "jest.config.js",
    "vitest.config.ts",
    "tailwind.config.js",
    "tailwind.config.ts",
    "postcss.config.js",
    "eslint.config.js",
    ".eslintrc.json",
    ".eslintrc.js",
    ".prettierrc",
    ".prettierrc.json",
    "tox.ini",
    "pytest.ini",
    "mypy.ini",
    "ruff.toml",
    // -- repository plumbing -----------------------------------------------
    ".gitignore",
    ".gitattributes",
    ".gitmodules",
    ".editorconfig",
    ".nvmrc",
    ".tool-versions",
    ".dockerignore",
    ".env.example",
    "Dockerfile",
    "docker-compose.yml",
    "docker-compose.yaml",
    // -- CI: opened for orientation, edited rarely -------------------------
    ".github/",
    ".circleci/",
    ".gitlab-ci.yml",
    "Jenkinsfile",
    "azure-pipelines.yml",
    ".travis.yml",
];

/// A set of path patterns that are treated as ubiquitous before the corpus can
/// speak for itself.
///
/// Cheap to construct, cheap to extend, and matched in `O(patterns)` with no
/// allocation. Ordering never reaches a caller — the only question asked of it is
/// a boolean — so this deliberately keeps insertion order rather than sorting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denylist {
    rules: Vec<Rule>,
}

/// One compiled [`Denylist`] pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rule {
    /// A bare file name, matched at any depth.
    Name(String),
    /// An extension, from a `*.ext` pattern.
    Extension(String),
    /// A directory prefix, from a pattern ending in `/`.
    Prefix(LogicalPath),
    /// A full logical path.
    Exact(LogicalPath),
}

impl Denylist {
    /// An empty denylist. Nothing is ubiquitous.
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Compiles a list of patterns. Unparseable patterns are skipped.
    pub fn from_patterns<I, S>(patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut list = Self::new();
        for p in patterns {
            list.push(p.as_ref());
        }
        list
    }

    /// Adds one pattern, in any of the four forms
    /// [`COLD_START_DENYLIST`] documents.
    ///
    /// Silently ignores a pattern that is not a valid relative path — an
    /// operator's config file is not a reason to fail to start.
    pub fn push(&mut self, pattern: &str) {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return;
        }
        let rule = if let Some(ext) = pattern.strip_prefix("*.") {
            if ext.is_empty() || ext.contains('/') {
                return;
            }
            Rule::Extension(ext.to_owned())
        } else if pattern.ends_with('/') || pattern.ends_with('\\') {
            match LogicalPath::new(pattern) {
                Ok(p) if !p.is_root() => Rule::Prefix(p),
                _ => return,
            }
        } else if pattern.contains('/') || pattern.contains('\\') {
            match LogicalPath::new(pattern) {
                Ok(p) if !p.is_root() => Rule::Exact(p),
                _ => return,
            }
        } else {
            Rule::Name(pattern.to_owned())
        };
        if !self.rules.contains(&rule) {
            self.rules.push(rule);
        }
    }

    /// True when the path is a shipped orientation file.
    pub fn matches(&self, path: &LogicalPath) -> bool {
        self.rules.iter().any(|rule| match rule {
            Rule::Name(name) => path
                .file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case(name)),
            Rule::Extension(ext) => path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case(ext)),
            Rule::Prefix(prefix) => path.starts_with(prefix),
            Rule::Exact(exact) => path == exact,
        })
    }

    /// How many patterns are compiled in.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// True when nothing is denylisted.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

impl Extend<String> for Denylist {
    fn extend<I: IntoIterator<Item = String>>(&mut self, iter: I) {
        for p in iter {
            self.push(&p);
        }
    }
}

/// The shipped list, compiled.
impl Default for Denylist {
    fn default() -> Self {
        Self::from_patterns(COLD_START_DENYLIST.iter().copied())
    }
}

/// Paths that are invisible to territory inference no matter what the corpus
/// says (PRD §6.1, as corrected).
///
/// A file the operator referenced with `@` is added to context **with no tool
/// call at all** — no `Read`, and no `PreToolUse` hook, including hooks matching
/// `Read`. Operator-pinned files therefore never appear as observations. The
/// only places they surface are `UserPromptSubmit`'s `prompt` field (which Polis
/// does not register, ADR-0044), the OTel `at_mention` event, and `attachment`
/// transcript records (ADR-0017).
pub const INVISIBLE_TO_INFERENCE: &str = "files referenced with @ produce no Read observation";

// ---------------------------------------------------------------------------
// SQLite plumbing
// ---------------------------------------------------------------------------

/// The key a path is stored under.
///
/// ASCII-lowercased, because [`LogicalPath`]'s `Eq` and `Ord` fold ASCII case
/// (ADR-0028) while SQLite's default `TEXT` collation is byte-exact. Storing the
/// display casing would split `SRC/Auth.ts` and `src/auth.ts` into two rows whose
/// counts the in-memory index would then merge, and `df` would disagree with
/// `SELECT COUNT(*)`. The corpus holds paths and counts only (ADR-0005), so
/// losing the display casing costs nothing.
fn store_key(path: &LogicalPath) -> String {
    path.as_str().to_ascii_lowercase()
}

/// Applies the connection pragmas. Every one is optional: a pragma that a
/// filesystem refuses (WAL over a network share is the usual one) is a
/// performance decision, never a reason to fail.
fn configure(conn: &Connection, wal: bool) {
    let _ = conn.busy_timeout(BUSY_TIMEOUT);
    if wal {
        // `journal_mode` returns the resulting mode as a row, so it cannot go
        // through `pragma_update`.
        let _: Result<String, _> = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0));
    }
    // The corpus is a quality signal, not critical state: a lost transaction on
    // a power cut costs one session out of five hundred, and is not worth an
    // fsync per commit on the world thread.
    let _ = conn.pragma_update(None, "synchronous", "NORMAL");
}

/// Reads `PRAGMA user_version`. This is also the first real read of the file, so
/// it is where a corrupt or non-database file announces itself.
fn schema_version(conn: &Connection) -> rusqlite::Result<u32> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(u32::try_from(v).unwrap_or(0))
}

/// Brings a store up to [`SCHEMA_VERSION`], one version at a time.
///
/// Every step is idempotent (`IF NOT EXISTS`) and runs inside one transaction
/// with the version bump, so an interrupted migration leaves either the old
/// schema or the new one and never a half-migrated store that the next launch
/// would have to guess about.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let mut version = schema_version(conn)?;
    while version < SCHEMA_VERSION {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match version {
            0 => conn.execute_batch(SCHEMA_V1)?,
            // Unreachable while SCHEMA_VERSION is 1; the arm exists so that
            // adding a version is a compile-time-obvious edit in one place.
            other => {
                conn.execute_batch("ROLLBACK")?;
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(1), // SQLITE_ERROR
                    Some(format!("no migration from corpus schema v{other}")),
                ));
            }
        }
        version += 1;
        conn.pragma_update(None, "user_version", version)?;
        conn.execute_batch("COMMIT")?;
    }
    Ok(())
}

/// Schema v1: one row per session, one row per `(session, path)`.
///
/// `ordinal` is `AUTOINCREMENT` rather than a timestamp so that "the last N
/// sessions" is an insertion order no clock change can perturb (PRD §7.4), and
/// so that a session id — which is not ordered — never has to be.
const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS session (
    ordinal    INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS observation (
    ordinal INTEGER NOT NULL,
    path    TEXT NOT NULL,
    PRIMARY KEY (ordinal, path)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS observation_path ON observation (path);
";

/// Loads `(session count, document frequencies)`.
///
/// `ORDER BY path` is not decoration: it makes the query's output stable across
/// SQLite's query planner deciding to use the index or a scan, which is the kind
/// of difference PRD §16's two-operating-system golden comparison would otherwise
/// surface as a layout diff.
fn load_index(conn: &Connection) -> rusqlite::Result<(u32, BTreeMap<LogicalPath, u32>)> {
    let sessions: i64 = conn.query_row("SELECT COUNT(*) FROM session", [], |row| row.get(0))?;
    let mut stmt =
        conn.prepare("SELECT path, COUNT(*) FROM observation GROUP BY path ORDER BY path")?;
    let mut df = BTreeMap::new();
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (path, count) = row?;
        // A row that is not a valid logical path cannot have been written by
        // this build. Skip it rather than refusing to open the store.
        if let Ok(path) = LogicalPath::new(&path) {
            df.insert(path, u32::try_from(count).unwrap_or(u32::MAX));
        }
    }
    Ok((u32::try_from(sessions).unwrap_or(u32::MAX), df))
}

/// Moves a corrupt store out of the way, and takes its sidecars with it.
///
/// The `-wal` and `-shm` files belong to the database they name; leaving them
/// behind would hand the fresh store a stale write-ahead log.
fn move_aside(path: &Path) -> std::io::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut from = path.as_os_str().to_owned();
        from.push(suffix);
        let from = PathBuf::from(from);
        if !from.exists() {
            continue;
        }
        let mut to = from.as_os_str().to_owned();
        to.push(".corrupt");
        // A previous corruption's file is overwritten: one generation of
        // forensics is enough, and unbounded `.corrupt.corrupt` files are not.
        let _ = std::fs::remove_file(&to);
        std::fs::rename(&from, PathBuf::from(to))?;
    }
    Ok(())
}

/// `%LOCALAPPDATA%\polis` — per-user, ACLed to the user by Windows, and present
/// on every Windows install. Not `%TEMP%`, which is world-writable in multi-user
/// configurations, and not `%APPDATA%`, which roams: a machine-local statistical
/// cache has no business on a domain profile share.
#[cfg(windows)]
pub(crate) fn state_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("polis"))
}

/// `$XDG_STATE_HOME/polis`, falling back to `~/.local/state/polis` — the XDG
/// default, which is unset far more often than the specification implies.
#[cfg(not(windows))]
pub(crate) fn state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("polis"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    fn sid(n: u32) -> SessionId {
        SessionId::from(format!("session-{n:04}"))
    }

    /// A corpus of `sessions` sessions where the first `readers` of them read
    /// `path`, plus a per-session unique path so every session has content.
    fn corpus_with(sessions: u32, path: &str, readers: u32) -> Corpus {
        let mut c = Corpus::in_memory().expect("in-memory corpus");
        for i in 0..sessions {
            let mut paths = vec![lp(&format!("src/unique/f{i}.rs"))];
            if i < readers {
                paths.push(lp(path));
            }
            c.record_session(&sid(i), &paths).expect("record");
        }
        c
    }

    // -- the weighting math ------------------------------------------------

    #[test]
    fn idf_is_one_for_a_path_in_a_single_session_and_the_floor_for_one_in_all() {
        // Read by exactly one session out of many: maximally informative, and
        // the normalisation makes that exactly 1.0 rather than ln(N).
        assert!((idf(100, 1) - 1.0).abs() < 1e-6);
        // Read by every session: floored, never zero.
        assert!((idf(100, 100) - MIN_DISCOUNT).abs() < 1e-6);
        assert!((idf(20, 20) - MIN_DISCOUNT).abs() < 1e-6);
        // Never amplifies. ADR-0020's iso thresholds are absolute.
        for n in [2_u32, 3, 20, 100, 1_000, 10_000] {
            for df in [0_u32, 1, 2, n / 2, n - 1, n, n + 5] {
                let v = idf(n, df);
                assert!(
                    (MIN_DISCOUNT..=1.0).contains(&v),
                    "idf({n}, {df}) = {v} escaped (0, 1]"
                );
            }
        }
    }

    #[test]
    fn idf_matches_the_prd_worked_example() {
        // PRD §6.1: "A path read by 90% of sessions contributes ~nothing; one
        // read by 3% dominates."
        let ubiquitous = idf(1_000, 900);
        let rare = idf(1_000, 30);
        assert!(ubiquitous < 0.02, "90% of sessions: {ubiquitous}");
        assert!(rare > 0.4, "3% of sessions: {rare}");
        assert!(
            rare / ubiquitous > 20.0,
            "the rare path must dominate: {rare} vs {ubiquitous}"
        );
        // Monotone in df, which is the property the whole thing rests on.
        let mut previous = f32::INFINITY;
        for df in 1..=1_000 {
            let v = idf(1_000, df);
            assert!(v <= previous, "idf must not increase with df (df = {df})");
            previous = v;
        }
    }

    #[test]
    fn idf_is_total_on_the_degenerate_corpora() {
        // An empty corpus discounts nothing.
        assert!((idf(0, 0) - 1.0).abs() < 1e-6);
        assert!((idf(1, 1) - 1.0).abs() < 1e-6);
        // A path the corpus has never seen (df = 0) is maximally informative.
        assert!((idf(500, 0) - 1.0).abs() < 1e-6);
        // df > N is impossible but must not produce NaN or a negative weight.
        let v = idf(10, 40);
        assert!(v.is_finite() && v >= MIN_DISCOUNT, "{v}");
    }

    #[test]
    fn an_empty_corpus_discounts_nothing_it_does_not_recognise() {
        let c = Corpus::in_memory().expect("in-memory corpus");
        assert_eq!(c.session_count(), 0);
        assert_eq!(c.regime(), Regime::ColdStart);
        assert!((c.ubiquity_discount(&lp("src/auth/session.rs")) - 1.0).abs() < 1e-6);
        // ...but the shipped denylist still answers for the README.
        assert!((c.ubiquity_discount(&lp("README.md")) - DENYLIST_DISCOUNT).abs() < 1e-6);
    }

    // -- cold start and the transition -------------------------------------

    #[test]
    fn the_denylist_hands_over_to_the_corpus_at_the_documented_session_count() {
        // One session short of the threshold: still the denylist, so a path
        // every one of those sessions read is *not* yet discounted.
        let c = corpus_with(
            COLD_START_SESSIONS - 1,
            "src/hot.rs",
            COLD_START_SESSIONS - 1,
        );
        assert_eq!(c.regime(), Regime::ColdStart);
        assert!((c.ubiquity_discount(&lp("src/hot.rs")) - 1.0).abs() < 1e-6);

        // One more session and the measured counts take over.
        let c = corpus_with(COLD_START_SESSIONS, "src/hot.rs", COLD_START_SESSIONS);
        assert_eq!(c.regime(), Regime::Learned);
        assert!(
            (c.ubiquity_discount(&lp("src/hot.rs")) - MIN_DISCOUNT).abs() < 1e-6,
            "a path every session read must collapse once the corpus is trusted"
        );
        // And a path only one session read is undiscounted.
        assert!((c.ubiquity_discount(&lp("src/unique/f3.rs")) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_learned_corpus_can_overrule_the_shipped_denylist() {
        // A repository where nobody reads the README: once learned, it is no
        // longer discounted, which a denylist used as a permanent floor could
        // never express.
        let c = corpus_with(COLD_START_SESSIONS, "README.md", 1);
        assert_eq!(c.regime(), Regime::Learned);
        let readme = c.ubiquity_discount(&lp("README.md"));
        assert!(
            readme > 0.9,
            "the denylist must stop applying once the corpus speaks: {readme}"
        );
    }

    #[test]
    fn the_denylist_understands_all_four_pattern_forms() {
        let d = Denylist::default();
        // name, at any depth, case-insensitively
        assert!(d.matches(&lp("README.md")));
        assert!(d.matches(&lp("crates/core/readme.md")));
        assert!(d.matches(&lp("package.json")));
        // extension
        assert!(d.matches(&lp("Cargo.lock")));
        assert!(d.matches(&lp("deep/nested/thing.LOCK")));
        // directory prefix
        assert!(d.matches(&lp(".github/workflows/ci.yml")));
        // and the things that must NOT match
        assert!(!d.matches(&lp("src/main.rs")));
        assert!(!d.matches(&lp("src/readme_parser.rs")));
        assert!(!d.matches(&lp("docs/architecture.md")));
        assert!(!d.matches(&lp("githubbed/thing.rs")));
    }

    #[test]
    fn the_denylist_is_extensible_and_survives_junk_patterns() {
        let mut d = Denylist::new();
        assert!(d.is_empty());
        d.push("HOUSE_STYLE.md");
        d.push("ops/runbook.md");
        d.push("generated/");
        d.push("*.snap");
        let before = d.len();
        // Junk is skipped, never panics, and never fails a startup.
        d.push("");
        d.push("   ");
        d.push("*.");
        d.push("/absolute/path");
        d.push("HOUSE_STYLE.md"); // duplicate
        assert_eq!(d.len(), before, "junk and duplicates must not add rules");

        assert!(d.matches(&lp("docs/HOUSE_STYLE.md")));
        assert!(d.matches(&lp("ops/runbook.md")));
        assert!(!d.matches(&lp("other/runbook.md")), "exact paths are exact");
        assert!(d.matches(&lp("generated/api/types.ts")));
        assert!(d.matches(&lp("tests/snapshots/x.snap")));
        assert!(
            !d.matches(&lp("README.md")),
            "a fresh denylist ships nothing"
        );

        // And the whole list is replaceable on a live corpus.
        let c = Corpus::in_memory().expect("corpus").with_denylist(d);
        assert!((c.ubiquity_discount(&lp("README.md")) - 1.0).abs() < 1e-6);
        assert!((c.ubiquity_discount(&lp("ops/runbook.md")) - DENYLIST_DISCOUNT).abs() < 1e-6);
    }

    // -- recording, dedup, pruning -----------------------------------------

    #[test]
    fn a_path_read_twice_in_one_session_counts_once_and_a_resumed_session_does_not_double() {
        let mut c = Corpus::in_memory().expect("corpus");
        let paths = [lp("src/a.rs"), lp("src/a.rs"), lp("src/b.rs")];
        c.record_session(&sid(1), &paths).expect("record");
        assert_eq!(c.session_count(), 1);
        assert_eq!(c.sessions_that_read(&lp("src/a.rs")), 1);

        // The same session recorded again: no new session, no new count, but a
        // path it had not contributed before is still learned.
        c.record_session(&sid(1), &[lp("src/a.rs"), lp("src/c.rs")])
            .expect("record");
        assert_eq!(c.session_count(), 1, "a resumed session is one session");
        assert_eq!(c.sessions_that_read(&lp("src/a.rs")), 1);
        assert_eq!(c.sessions_that_read(&lp("src/c.rs")), 1);
        assert_eq!(c.path_count(), 3);
    }

    #[test]
    fn case_folded_paths_are_one_row_not_two() {
        let mut c = Corpus::in_memory().expect("corpus");
        c.record_session(&sid(1), &[lp("SRC/Auth.ts")]).expect("a");
        c.record_session(&sid(2), &[lp("src/auth.ts")]).expect("b");
        assert_eq!(
            c.sessions_that_read(&lp("src/AUTH.ts")),
            2,
            "ADR-0028 folds ASCII case; the store must agree"
        );
        assert_eq!(c.path_count(), 1);
    }

    #[test]
    fn pruning_keeps_the_last_n_sessions_and_rebuilds_the_counts() {
        let mut c = Corpus::in_memory().expect("corpus");
        for i in 0..10 {
            c.record_session(&sid(i), &[lp("src/always.rs"), lp(&format!("src/f{i}.rs"))])
                .expect("record");
        }
        assert_eq!(c.session_count(), 10);
        assert_eq!(c.sessions_that_read(&lp("src/always.rs")), 10);

        let dropped = c.prune(4).expect("prune");
        assert_eq!(dropped, 6);
        assert_eq!(c.session_count(), 4);
        assert_eq!(c.sessions_that_read(&lp("src/always.rs")), 4);
        assert_eq!(c.sessions_that_read(&lp("src/f0.rs")), 0, "oldest dropped");
        assert_eq!(c.sessions_that_read(&lp("src/f9.rs")), 1, "newest kept");
        assert_eq!(c.path_count(), 5, "always.rs plus f6..f9");

        // Pruning to more than there is, is a no-op.
        assert_eq!(c.prune(100).expect("prune"), 0);
        assert_eq!(c.session_count(), 4);
        // Pruning to nothing empties it without breaking it.
        assert_eq!(c.prune(0).expect("prune"), 4);
        assert_eq!(c.session_count(), 0);
        assert_eq!(c.path_count(), 0);
    }

    #[test]
    fn recording_prunes_itself_to_the_window() {
        let mut c = Corpus::in_memory().expect("corpus");
        c.set_window(5);
        for i in 0..40 {
            c.record_session(&sid(i), &[lp(&format!("src/f{i}.rs"))])
                .expect("record");
        }
        assert_eq!(c.session_count(), 5, "growth is bounded without the caller");
        assert_eq!(c.path_count(), 5);
        assert_eq!(c.sessions_that_read(&lp("src/f39.rs")), 1);
    }

    #[test]
    fn document_frequencies_come_back_in_path_order() {
        let mut c = Corpus::in_memory().expect("corpus");
        c.record_session(
            &sid(1),
            &[lp("zeta/z.rs"), lp("alpha/a.rs"), lp("middle/m.rs")],
        )
        .expect("record");
        let order: Vec<&str> = c.document_frequencies().map(|(p, _)| p.as_str()).collect();
        assert_eq!(order, ["alpha/a.rs", "middle/m.rs", "zeta/z.rs"]);
    }

    // -- persistence -------------------------------------------------------

    #[test]
    fn counts_survive_a_reopen_and_the_schema_is_versioned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("corpus.db");

        {
            let mut c = Corpus::open(&path).expect("open");
            for i in 0..3 {
                c.record_session(&sid(i), &[lp("src/a.rs")]).expect("write");
            }
        }
        let c = Corpus::open(&path).expect("reopen");
        assert_eq!(c.session_count(), 3);
        assert_eq!(c.sessions_that_read(&lp("src/a.rs")), 3);
        assert!(!c.is_degraded());

        // Reopening is also the migration path, and it is idempotent.
        let conn = Connection::open(&path).expect("raw open");
        assert_eq!(schema_version(&conn).expect("version"), SCHEMA_VERSION);
        migrate(&conn).expect("re-migrate");
        assert_eq!(schema_version(&conn).expect("version"), SCHEMA_VERSION);
    }

    #[test]
    fn a_store_from_a_newer_polis_is_refused_and_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("corpus.db");
        {
            let mut c = Corpus::open(&path).expect("open");
            c.record_session(&sid(1), &[lp("src/a.rs")]).expect("write");
            drop(c);
            let conn = Connection::open(&path).expect("raw open");
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 7)
                .expect("bump");
        }
        assert!(Corpus::open(&path).is_err());
        let c = Corpus::open_or_degraded(&path);
        assert!(c.is_degraded());
        assert!(
            c.degraded_reason().unwrap_or_default().contains("schema"),
            "{:?}",
            c.degraded_reason()
        );
        // Never moved aside: a newer build's data is not ours to destroy.
        assert!(path.exists());
        assert!(!path.with_extension("db.corrupt").exists());
        let conn = Connection::open(&path).expect("raw open");
        assert_eq!(schema_version(&conn).expect("version"), SCHEMA_VERSION + 7);
    }

    #[test]
    fn a_corrupt_database_never_stops_polis_starting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("corpus.db");
        std::fs::write(&path, b"this is not a database, it is a picture of a cat").expect("write");

        // The fallible constructor reports it...
        assert!(Corpus::open(&path).is_err());

        // ...and the startup constructor recovers: the junk is moved aside and a
        // fresh, working store takes its place.
        let mut c = Corpus::open_or_degraded(&path);
        assert!(!c.is_degraded(), "{:?}", c.degraded_reason());
        assert_eq!(c.regime(), Regime::ColdStart);
        c.record_session(&sid(1), &[lp("src/a.rs")]).expect("write");
        assert_eq!(c.sessions_that_read(&lp("src/a.rs")), 1);

        let aside = dir.path().join("corpus.db.corrupt");
        assert!(aside.exists(), "the corrupt file is kept for forensics");
        assert_eq!(
            std::fs::read(&aside).expect("read"),
            b"this is not a database, it is a picture of a cat"
        );
        // Reopening the fresh store works and does not re-trigger recovery.
        let c = Corpus::open(&path).expect("reopen");
        assert_eq!(c.session_count(), 1);
    }

    #[test]
    fn a_busy_store_degrades_without_destroying_anything() {
        // Classification, not the filesystem: this is the decision that keeps a
        // healthy store from being deleted because a sibling instance had it
        // open, and it must be exercised directly.
        let busy = OpenFailure::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(5), // SQLITE_BUSY
            Some("database is locked".to_owned()),
        ));
        let locked = OpenFailure::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(6), // SQLITE_LOCKED
            None,
        ));
        let not_a_db = OpenFailure::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(26), // SQLITE_NOTADB
            None,
        ));
        let corrupt = OpenFailure::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(11), // SQLITE_CORRUPT
            None,
        ));
        assert!(
            !busy.is_corruption(),
            "a locked store is not a broken store"
        );
        assert!(!locked.is_corruption());
        assert!(!OpenFailure::FutureSchema(9).is_corruption());
        assert!(!OpenFailure::Io(std::io::Error::other("nope")).is_corruption());
        assert!(not_a_db.is_corruption());
        assert!(corrupt.is_corruption());

        // And a degraded corpus is fully usable, just uninformed.
        let mut c = Corpus::degraded("test");
        assert_eq!(c.regime(), Regime::Degraded);
        assert!(c.regime().is_cold());
        c.record_session(&sid(1), &[lp("src/a.rs")])
            .expect("a degraded write is dropped, not an error");
        assert_eq!(c.session_count(), 0);
        assert_eq!(c.prune(1).expect("prune"), 0);
        c.reload().expect("reload");
        assert!((c.ubiquity_discount(&lp("src/a.rs")) - 1.0).abs() < 1e-6);
        assert!((c.ubiquity_discount(&lp("Cargo.toml")) - DENYLIST_DISCOUNT).abs() < 1e-6);
    }

    #[test]
    fn two_instances_can_hold_the_same_store_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("corpus.db");

        let mut a = Corpus::open(&path).expect("first instance");
        let mut b = Corpus::open_or_degraded(&path);
        assert!(!b.is_degraded(), "{:?}", b.degraded_reason());

        a.record_session(&sid(1), &[lp("src/a.rs")]).expect("a");
        b.record_session(&sid(2), &[lp("src/b.rs")]).expect("b");
        a.record_session(&sid(3), &[lp("src/a.rs")]).expect("a");

        // Each instance's cached index only knows its own writes...
        assert_eq!(a.session_count(), 2);
        assert_eq!(b.session_count(), 1);
        // ...until it asks, and then both agree with the store.
        a.reload().expect("reload a");
        b.reload().expect("reload b");
        assert_eq!(a.session_count(), 3);
        assert_eq!(b.session_count(), 3);
        assert_eq!(b.sessions_that_read(&lp("src/a.rs")), 2);
        assert_eq!(a.sessions_that_read(&lp("src/b.rs")), 1);

        // A third instance opening cold sees everything.
        let c = Corpus::open(&path).expect("third instance");
        assert_eq!(c.session_count(), 3);
    }

    #[test]
    fn the_default_path_is_platform_correct_and_never_a_temp_directory() {
        let Some(path) = Corpus::default_path() else {
            // No HOME and no LOCALAPPDATA is a legitimate environment; the
            // caller degrades rather than inventing a path.
            return;
        };
        assert!(path.is_absolute(), "{}", path.display());
        assert!(path.ends_with("polis/corpus.db") || path.ends_with("polis\\corpus.db"));
        assert_eq!(
            Corpus::default_state_dir().expect("state dir"),
            path.parent().expect("parent")
        );
    }

    #[test]
    fn recording_is_independent_of_the_order_paths_arrive_in() {
        // PRD §7.4: nothing about the persisted artefact may depend on iteration
        // order. Two sessions that read the same set in different orders must
        // leave the store in the same state.
        let forward = [lp("src/a.rs"), lp("src/b.rs"), lp("src/c.rs")];
        let backward = [lp("src/c.rs"), lp("src/b.rs"), lp("src/a.rs")];

        let mut one = Corpus::in_memory().expect("corpus");
        one.record_session(&sid(1), &forward).expect("record");
        let mut two = Corpus::in_memory().expect("corpus");
        two.record_session(&sid(1), &backward).expect("record");

        let a: Vec<(String, u32)> = one
            .document_frequencies()
            .map(|(p, n)| (p.as_str().to_owned(), n))
            .collect();
        let b: Vec<(String, u32)> = two
            .document_frequencies()
            .map(|(p, n)| (p.as_str().to_owned(), n))
            .collect();
        assert_eq!(a, b);
    }
}
