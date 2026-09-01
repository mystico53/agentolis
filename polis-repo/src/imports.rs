//! The import graph, which becomes the streets (PRD §9).
//!
//! > Streets are **cross-district import relationships only**. Intra-district
//! > coupling is expected and boring; what crosses a boundary is the
//! > architecturally interesting thing.
//!
//! > **Do not let the import graph fight the directory tree for position.** The
//! > tree determines placement […] Imports act only as a weak attraction force
//! > *within* a district, and as drawn edges everywhere else.
//!
//! Extraction is `tree-sitter` per language, run once at index time and
//! incrementally on file change. **Failure to parse a file is non-fatal** — that
//! file simply has no streets.

use polis_events::LogicalPath;

/// The whole-repository import graph.
#[derive(Debug, Default)]
pub struct ImportGraph {
    _private: (),
}

impl ImportGraph {
    /// Extracts imports for every file the grammar set understands.
    pub fn build(tree: &crate::RepoTree) -> Self {
        let _ = tree;
        todo!("PRD §9 — tree-sitter per language, parse failure is non-fatal")
    }

    /// Re-extracts one file after it changed.
    pub fn update_file(&mut self, path: &LogicalPath, source: &str) {
        let _ = (path, source);
        todo!("PRD §9 — incremental re-extraction on file change")
    }

    /// Import edges whose endpoints are in different districts. These, and only
    /// these, are drawn as streets.
    pub fn cross_district_edges(&self) -> Vec<Street> {
        todo!("PRD §9 — group by parent district, keep only the crossings")
    }

    /// Files in the top decile of inbound import count — one of the signals
    /// behind a monument (PRD §8).
    pub fn inbound_top_decile(&self) -> Vec<LogicalPath> {
        todo!("PRD §8 — monuments are the wayfinding layer, not polish")
    }
}

/// A drawn street between two districts (PRD §9).
#[derive(Debug, Clone)]
pub struct Street {
    /// The district the edges leave.
    pub from: LogicalPath,
    /// The district they arrive at.
    pub to: LogicalPath,
    /// Distinct import edges carried. Street width is proportional to this.
    pub edge_count: u32,
}

/// Which tree-sitter grammar to use for a file, by extension.
///
/// `None` means no grammar is loaded for that language, which is a normal
/// condition: that file has no streets and nothing else changes.
pub fn grammar_for(path: &LogicalPath) -> Option<Language> {
    let _ = path;
    todo!("PRD §9 — extension to grammar, unknown is not an error")
}

/// A language with a loaded tree-sitter grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    /// Rust.
    Rust,
    /// TypeScript and TSX.
    TypeScript,
    /// JavaScript and JSX.
    JavaScript,
    /// Python.
    Python,
    /// Go.
    Go,
}
