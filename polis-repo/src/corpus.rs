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
//! orientation files.

use polis_events::LogicalPath;

/// The rolling per-path session-read counts.
#[derive(Debug)]
pub struct Corpus {
    _private: (),
}

impl Corpus {
    /// Opens or creates the store.
    pub fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        let _ = path;
        todo!("PRD §6.1 — SQLite at the platform state directory")
    }

    /// Records that one session read a set of paths. Called once per session, at
    /// its end — not per read, which would make ubiquity a function of how
    /// chatty an agent is rather than how common the file is.
    pub fn record_session(&mut self, paths: &[LogicalPath]) -> anyhow::Result<()> {
        let _ = paths;
        todo!("PRD §6.1 — one row per (session, path), rolled forward over N sessions")
    }

    /// The `log(N / sessions_that_read_path)` multiplier.
    ///
    /// Returns 1.0 when the corpus is empty, so a cold start behaves as if every
    /// path were equally informative rather than silently zeroing every weight.
    pub fn ubiquity_discount(&self, path: &LogicalPath) -> f32 {
        let _ = path;
        todo!("PRD §6.1 — log(N / df), clamped")
    }

    /// How many sessions the corpus covers. Below a threshold, callers should
    /// prefer [`COLD_START_DENYLIST`].
    pub fn session_count(&self) -> u32 {
        todo!("PRD §6.1 — the N in log(N / df)")
    }
}

/// Paths that are orientation reads in essentially every repository.
///
/// Used only until the corpus has enough sessions to speak for itself.
pub const COLD_START_DENYLIST: &[&str] = &[
    "README.md",
    "README",
    "package.json",
    "Cargo.toml",
    "Cargo.lock",
    "tsconfig.json",
    "go.mod",
    "pyproject.toml",
    "CLAUDE.md",
    ".gitignore",
    "Makefile",
];

/// Paths that are invisible to territory inference no matter what the corpus
/// says (PRD §6.1, as corrected).
///
/// A file the operator referenced with `@` is added to context **with no tool
/// call at all** — no `Read`, and no `PreToolUse` hook, including hooks matching
/// `Read`. Operator-pinned files therefore never appear as observations. The
/// only places they surface are `UserPromptSubmit`'s `prompt` field, the OTel
/// `at_mention` event, and `attachment` transcript records (ADR-0017).
pub const INVISIBLE_TO_INFERENCE: &str = "files referenced with @ produce no Read observation";
