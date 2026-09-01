//! Landmark and zone classification (PRD §8).
//!
//! > Organic cities are harder to navigate than grids — this is true of the real
//! > ones, and it is the price of the look. The mitigation is the same one that
//! > makes Venice navigable: **landmarks carry the wayfinding, not addresses.**
//! > Invest here. […] Landmarks are the mitigation and must not be treated as
//! > polish.

use polis_events::LogicalPath;

/// How the landmark layer treats a file (PRD §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FileClass {
    /// An ordinary building.
    #[default]
    Ordinary,
    /// An entry point: `main`, `index`, a route table, a CLI root, or a file in
    /// the top decile of inbound imports.
    ///
    /// Rendered tall with a distinct silhouette and **labelled at every zoom**.
    /// These are the orientation anchors — the first thing the eye finds when
    /// zoomed out.
    Monument,
    /// `node_modules`, vendored, generated, `target/`.
    ///
    /// Large, uniform, deliberately dull, drawn as a **single mass** rather than
    /// individual buildings. Making these boring is the feature: the eye should
    /// slide off them.
    Industrial,
    /// Repo root and top-level config — a recognisable open space at the
    /// historic centre.
    CivicSquare,
}

/// Classifies a file.
///
/// Deterministic and path-only, so it can run before any git or import data
/// exists and cannot make the layout depend on ingest timing (PRD §7.4).
pub fn classify(path: &LogicalPath) -> FileClass {
    let _ = path;
    todo!("PRD §8 — entry points, industrial trees, and the civic square")
}

/// True once a file has gone 90 days untouched (PRD §8, §7.5).
///
/// Rendered desaturated with a softened outline and encroaching vegetation.
/// Together with vacant lots this makes dead code visible without anyone running
/// an analysis.
pub fn is_overgrown(last_commit_at: i64, now: i64) -> bool {
    let _ = (last_commit_at, now);
    todo!("PRD §8 — 90 days")
}
