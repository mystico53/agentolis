//! The lock-free snapshot the renderer reads (PRD §5).
//!
//! > Single-writer, multi-reader. One thread owns `World` and applies events;
//! > the renderer reads a lock-free snapshot (`arc-swap`) **published at most
//! > once per frame**.
//!
//! > **Decouple event rate from frame rate.** Never render on event arrival.
//!
//! The publish cadence is the load-bearing part. A snapshot per event would put
//! the ingest rate straight into the render thread's lap and defeat PRD §13.1's
//! sub-2% idle CPU budget; a snapshot per batch keeps the renderer sampling at
//! its own pace.

use std::sync::Arc;

use polis_layout::CityLayout;

use crate::{Thread, World};

/// What the renderer sees. Immutable once published.
#[derive(Debug)]
pub struct WorldSnapshot {
    /// The city. Shared by `Arc` rather than cloned: it changes on the order of
    /// seconds while snapshots publish per frame, and the base map is cached to
    /// a texture keyed on its identity (PRD §13).
    pub layout: Arc<CityLayout>,
    /// Threads, already ordered for the status rail.
    pub threads: Vec<Thread>,
    /// Attention marks in PRD §11.1 order.
    pub attention: Vec<crate::attention::Attention>,
    /// Channel health, for the status bar.
    pub health: crate::Health,
}

/// Builds the publish/subscribe pair, in the shape of
/// [`std::sync::mpsc::channel`].
pub fn channel(initial: WorldSnapshot) -> (SnapshotPublisher, SnapshotReader) {
    // Taken by value because the cell stores it. Dropped for now.
    drop(initial);
    todo!("PRD §5 — arc_swap::ArcSwap<WorldSnapshot>")
}

/// The writer half. Owned by the world thread.
#[derive(Debug)]
pub struct SnapshotPublisher {
    _private: (),
}

impl SnapshotPublisher {
    /// Publishes the current world. Call **once per batch**, never per event.
    pub fn publish(&self, world: &World) {
        let _ = world;
        todo!("PRD §5 — build the snapshot, then ArcSwap::store")
    }
}

/// The reader half. Cloneable and cheap; the renderer loads it per frame.
#[derive(Debug, Clone)]
pub struct SnapshotReader {
    _private: (),
}

impl SnapshotReader {
    /// Loads the newest published snapshot. Never blocks and never allocates.
    pub fn load(&self) -> Arc<WorldSnapshot> {
        todo!("PRD §5 — ArcSwap::load_full")
    }
}
