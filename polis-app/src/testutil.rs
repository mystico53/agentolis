//! Scratch directories for tests.
//!
//! `polis-app` declares no dev-dependencies and no agent may add one, so there
//! is no `tempfile` here. This is the eight-line replacement: a per-process,
//! per-tag directory under the platform temp directory, emptied on creation so a
//! re-run never sees a previous run's files.

use std::path::PathBuf;

/// A fresh, empty directory named for `tag` and this process.
///
/// Keyed on the process id rather than a counter or a clock: two tests running
/// concurrently in the same process get different tags, and two `cargo test`
/// processes get different pids, which is the only collision that matters.
pub(crate) fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("polis-app-tests")
        .join(format!("{}-{tag}", std::process::id()));
    // Best-effort: a leftover from a killed run must not fail the test that is
    // about to overwrite it anyway.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temp directory is creatable");
    dir
}
