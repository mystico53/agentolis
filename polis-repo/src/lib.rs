//! `polis-repo` — the repository index (PRD §6.1, §7.1, §9, §14).
//!
//! Everything the city is *made of*, before anything is laid out: the file tree,
//! the growth order that gives the city its age structure, the cross-district
//! import graph that becomes its streets, and the corpus statistics that stop
//! every agent's territory from claiming the README.
//!
//! | Module | PRD section |
//! |---|---|
//! | [`git`] | §7.1 growth order from `git log` |
//! | [`imports`] | §9 tree-sitter import extraction |
//! | [`corpus`] | §6.1 the TF-IDF ubiquity discount |
//! | [`classify`] | §8 landmark and industrial-zone classification |

pub mod classify;
pub mod corpus;
pub mod git;
pub mod imports;

use std::collections::BTreeMap;
use std::path::PathBuf;

use polis_events::{LogicalPath, WorktreeId};

/// The file tree the city is generated from (PRD §5, `World::repo`).
///
/// `BTreeMap`, not `HashMap`: PRD §7.4 requires that iteration order can never
/// reach the layout, and this map's order does.
#[derive(Debug, Clone, Default)]
pub struct RepoTree {
    /// Absolute path of the primary checkout.
    pub root: PathBuf,
    /// Every tracked file, ordered.
    pub files: BTreeMap<LogicalPath, FileFacts>,
    /// Registered worktrees (PRD §7.6). One shared base map, tinted per
    /// worktree — never one city each.
    pub worktrees: BTreeMap<WorktreeId, PathBuf>,
}

/// What the index knows about one file before any agent touches it.
#[derive(Debug, Clone, Default)]
pub struct FileFacts {
    /// Size in bytes. Footprint area is proportional to its square root,
    /// clamped to `[min_lot, block_area * 0.6]` (PRD §7.3).
    pub size_bytes: u64,
    /// Position in the growth sequence — the commit index that first added this
    /// file. Files added in the repo's first year form the old town; files added
    /// last month sit on the periphery (PRD §7.1).
    pub growth_index: u32,
    /// Commit timestamp of that first addition, seconds since the epoch.
    pub added_at: i64,
    /// Last commit that touched it. Drives overgrowth at 90 days (PRD §8).
    pub last_commit_at: i64,
    /// How the landmark layer should treat it.
    pub class: classify::FileClass,
}

/// Builds and incrementally maintains a [`RepoTree`].
#[derive(Debug)]
pub struct RepoIndex {
    _private: (),
}

impl RepoIndex {
    /// Indexes a repository from scratch.
    ///
    /// PRD §13.1 budgets cold start → first frame at under 3 s for a 5 000-file
    /// repo, and this is most of it, so the git walk is cached (see
    /// [`git::GrowthSequence`]).
    pub fn open(root: &std::path::Path) -> anyhow::Result<Self> {
        let _ = root;
        todo!("PRD §7.1 — walk the tree, then replay git log for the growth order")
    }

    /// The current tree.
    pub fn tree(&self) -> &RepoTree {
        todo!("PRD §5 — the repo half of World")
    }

    /// Applies new commits without recomputing the world.
    ///
    /// > Growth is genuinely incremental — a new file runs one growth step, it
    /// > does not regenerate the world. (PRD §7.4)
    pub fn refresh(&mut self) -> anyhow::Result<RepoDelta> {
        todo!("PRD §7.4 — incremental growth, cached on HEAD")
    }
}

/// What changed between two index states, so the layout can run one growth step.
#[derive(Debug, Clone, Default)]
pub struct RepoDelta {
    /// Files that appeared.
    pub added: Vec<LogicalPath>,
    /// Files that vanished. They leave vacant lots that go to seed (PRD §7.5),
    /// never holes.
    pub removed: Vec<LogicalPath>,
    /// Files whose size changed enough to matter.
    pub resized: Vec<LogicalPath>,
}
