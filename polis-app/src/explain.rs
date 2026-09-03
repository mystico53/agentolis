//! The explanation of the map, in one place (PRD §10, §12).
//!
//! Polis has four ways in — bare `polis`, `polis map`, `polis watch`, a
//! double-clicked `polis.exe` — and until this module existed the four sentences
//! that say what a building is were printed by exactly one of them, on stdout,
//! which is the surface the other three never show. A first-run review put it
//! plainly: *"you can pan and zoom a dark diagram and that is all"*.
//!
//! So the strings live here, once, and both the terminal screen
//! ([`crate::setup::orientation`]) and the window ([`crate::ui::help`] and the
//! first-run overlay) render the same words. Changing the metaphor means
//! changing it here and nowhere else.
//!
//! # The dismissal is remembered beside the first-run marker
//!
//! [`remember_dismissed`] writes `map-explained` into the state directory that
//! already holds `first-run` (`%LOCALAPPDATA%\polis` on Windows). A failed write
//! is silent and costs one extra overlay, which is not worth an error message on
//! the way to a window.

use std::path::{Path, PathBuf};

/// What the city is, in the fewest sentences that still answer the question.
///
/// Wrapped for a terminal at 78 columns, which is also a comfortable measure in
/// the window, so one array serves both.
pub const MAP: &[&str] = &[
    "Every building is a file. Every district is a directory.",
    "A building's height is its uncommitted work, so the tallest tower",
    "is the biggest unreviewed pile — the skyline points at what needs you.",
    "The old, dense core is the code you wrote first; the loose outskirts",
    "are last month's.",
];

/// [`MAP`] as one paragraph, for a surface that wraps text itself.
///
/// The array is wrapped for a terminal; a window is a different width every
/// time, so it re-wraps rather than inheriting a 78-column ragged edge.
pub fn paragraph() -> String {
    MAP.join(" ")
}

/// The same thing as a table, for the surfaces that have room for one.
///
/// The map view uses it as a legend; the terminal does not print it, because
/// [`MAP`] already says it in prose and saying it twice on one screen is noise.
pub const READING: &[(&str, &str)] = &[
    ("a building", "one file"),
    (
        "a district",
        "one folder — its buildings are the files in it",
    ),
    (
        "height",
        "uncommitted work: the tallest tower is the biggest unreviewed pile",
    ),
    (
        "the dense core",
        "the code written first; the loose outskirts are the newest",
    ),
    (
        "a cloud",
        "an agent, over the part of the tree it is working in",
    ),
];

/// The one line that says what the operator can do about the overlay.
pub const DISMISS: &str = "press any key or click to dismiss · h brings it back";

/// The name of the marker file, beside `first-run`.
const MARKER: &str = "map-explained";

/// The state directory's marker path, when there is a state directory.
fn marker() -> Option<PathBuf> {
    crate::config::Config::default_state_dir().map(|dir| dir.join(MARKER))
}

/// Whether the map explainer has already been dismissed on this machine.
pub fn dismissed() -> bool {
    marker().is_some_and(|p| p.exists())
}

/// Records that it has. Failure is silent — see the module docs.
pub fn remember_dismissed() {
    let Some(path) = marker() else { return };
    let Some(parent) = path.parent() else { return };
    remember_dismissed_in(parent);
}

/// [`dismissed`] against an explicit state directory, for tests.
pub fn dismissed_in(state_dir: &Path) -> bool {
    state_dir.join(MARKER).exists()
}

/// [`remember_dismissed`] against an explicit state directory, for tests.
pub fn remember_dismissed_in(state_dir: &Path) {
    let _ = std::fs::create_dir_all(state_dir);
    let _ = std::fs::write(
        state_dir.join(MARKER),
        "the map explainer has been shown and dismissed\n",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reviewer's actual complaint was that the words exist and are good and
    /// nothing shows them. Whatever else changes, these four claims are the
    /// product's whole metaphor and must survive editing.
    #[test]
    fn the_explanation_says_what_the_three_things_are() {
        let text = MAP.join(" ");
        for claim in [
            "building is a file",
            "district is a directory",
            "uncommitted work",
            "core is the code you wrote first",
        ] {
            assert!(text.contains(claim), "missing {claim:?} in:\n{text}");
        }
        // A first-run overlay is read in one glance or not at all.
        assert!(MAP.len() <= 6, "{} lines is not a glance", MAP.len());
        for line in MAP {
            assert!(line.chars().count() <= 78, "too wide to wrap well: {line}");
        }
        // No jargon: this is the first English a new operator reads.
        for jargon in ["OTLP", "territory", "PRD", "kernel", "iso-contour"] {
            assert!(!text.contains(jargon), "jargon {jargon:?} in:\n{text}");
        }
    }

    #[test]
    fn the_dismissal_is_remembered_in_the_state_directory() {
        let dir = crate::testutil::scratch("explain-marker").join("state");
        assert!(!dismissed_in(&dir), "a fresh machine has not seen it");
        remember_dismissed_in(&dir);
        assert!(dismissed_in(&dir), "and afterwards it has");
        // Beside the first-run marker, not somewhere else.
        assert!(dir.join(MARKER).is_file());
    }

    /// A state directory that cannot be created must not stop the window
    /// opening, and must not leave the marker somewhere else instead — an
    /// earlier version of this test wrote `map-explained` into the crate root,
    /// where the next `polis map` grew a building for it.
    #[test]
    fn a_state_directory_that_cannot_be_written_is_silent() {
        let root = crate::testutil::scratch("explain-readonly");
        let blocked = root.join("a-file");
        std::fs::write(&blocked, "not a directory\n").expect("write");
        let inside = blocked.join("state");
        remember_dismissed_in(&inside);
        assert!(
            !dismissed_in(&inside),
            "nothing was written, and that is fine"
        );
        assert!(blocked.is_file(), "and the file in the way is untouched");
    }
}
