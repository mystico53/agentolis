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
//!
//! # Why a renderer cannot force a recompute
//!
//! This is enforced by the type system, not by convention:
//!
//! * [`SnapshotReader`] holds **one** field — the `ArcSwap` cell — and has
//!   **one** method, [`SnapshotReader::load`], which is an atomic pointer read.
//!   It owns no [`World`], no channel to the world thread, and nothing that
//!   computes. There is no method on it that could recompute anything.
//! * [`SnapshotPublisher`] contains a [`RefCell`] and is therefore `Send` but
//!   **not `Sync`**: it can be moved onto the world thread and cannot be shared
//!   with the renderer at all.
//! * `publish` is rate-limited by [`SnapshotPublisher::min_interval`], so even a
//!   pathological drain loop that publishes per event cannot drive the reader
//!   faster than a frame.
//!
//! The renderer therefore draws whatever was last published, at whatever cadence
//! it likes, and the world thread decides when the world changes.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use polis_events::{LogicalPath, ThreadId};
use polis_layout::CityLayout;

use crate::{FileState, Health, Thread, ThreadStatus, UnattributedWorker, World};

/// The floor between two publishes.
///
/// The primary decoupling is architectural — the world thread drains a *batch*
/// off the bus and publishes once — and this is the backstop that keeps a
/// mistake there from turning 500 events/sec into 500 publishes/sec. 8 ms is
/// below the frame time of every display the operator is plausibly using, so it
/// never makes the snapshot the stale link.
pub const MIN_PUBLISH_INTERVAL: Duration = Duration::from_millis(8);

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
    /// PRD §11.3's early warning: pairs of threads whose clouds overlap, worst
    /// first.
    ///
    /// Kept out of [`WorldSnapshot::attention`] on purpose — a signal that
    /// fires *before* anything is destroyed must not compete for the eye with
    /// the one that fires while it is. See
    /// [`crate::contention::TerritoryOverlap`].
    pub overlaps: Vec<crate::contention::TerritoryOverlap>,
    /// Channel health, for the status bar.
    pub health: Health,
    /// Per-file live state — the source of PRD §7.3's building height.
    ///
    /// `Arc` for the same reason the layout is: it changes when an edit lands,
    /// not when a frame is drawn, so most publishes reuse the previous map.
    pub files: Arc<BTreeMap<LogicalPath, FileState>>,
    /// Workers with no thread, for the uncertainty the status rail shows rather
    /// than hides.
    pub unattributed: Vec<UnattributedWorker>,
    /// Monotonic publish counter. A renderer can skip work when it has not
    /// changed.
    pub generation: u64,
    /// The world instant this snapshot describes.
    pub at: Instant,
}

impl WorldSnapshot {
    /// An empty snapshot over a city — what a window shows before any event.
    pub fn empty(layout: Arc<CityLayout>) -> Self {
        Self {
            layout,
            threads: Vec::new(),
            attention: Vec::new(),
            overlaps: Vec::new(),
            health: Health::default(),
            files: Arc::new(BTreeMap::new()),
            unattributed: Vec::new(),
            generation: 0,
            at: Instant::now(),
        }
    }

    /// One thread by id.
    pub fn thread(&self, id: &ThreadId) -> Option<&Thread> {
        self.threads.iter().find(|t| &t.id == id)
    }

    /// One file's live state.
    pub fn file(&self, path: &LogicalPath) -> Option<&FileState> {
        self.files.get(path)
    }

    /// Uncommitted diff lines for a file — PRD §7.3's building height input.
    pub fn diff_lines(&self, path: &LogicalPath) -> u32 {
        self.files.get(path).map_or(0, |f| f.diff_lines)
    }

    /// Threads blocked on a human, in status-rail order.
    ///
    /// > **Primary decision it accelerates:** *unblock* — get to the thread that
    /// > is waiting on a human. (PRD §1)
    pub fn waiting(&self) -> impl Iterator<Item = &Thread> + '_ {
        self.threads
            .iter()
            .filter(|t| t.status == ThreadStatus::Waiting)
    }

    /// Threads with a converged territory, paired with it, ready for
    /// [`crate::territory::visible_clouds`].
    pub fn territories(&self) -> Vec<(&Thread, &crate::territory::Territory)> {
        self.threads.iter().map(|t| (t, &t.territory)).collect()
    }
}

/// Builds the publish/subscribe pair, in the shape of
/// [`std::sync::mpsc::channel`].
pub fn channel(initial: WorldSnapshot) -> (SnapshotPublisher, SnapshotReader) {
    let layout = Arc::clone(&initial.layout);
    let files = Arc::clone(&initial.files);
    let generation = initial.generation;
    let cell = Arc::new(ArcSwap::from_pointee(initial));
    let publisher = SnapshotPublisher {
        cell: Arc::clone(&cell),
        min_interval: MIN_PUBLISH_INTERVAL,
        state: RefCell::new(PublishState {
            last: None,
            generation,
            layout,
            layout_generation: u64::MAX,
            files,
            files_generation: u64::MAX,
        }),
    };
    (publisher, SnapshotReader { cell })
}

/// Builds the pair from a world, publishing its current state immediately.
pub fn from_world(world: &World) -> (SnapshotPublisher, SnapshotReader) {
    let (publisher, reader) = channel(WorldSnapshot::empty(Arc::new(world.layout.clone())));
    publisher.force(world);
    (publisher, reader)
}

/// The writer half. Owned by the world thread.
///
/// `Send` but **not `Sync`** — it holds a [`RefCell`] — so the renderer cannot
/// hold one even by accident.
#[derive(Debug)]
pub struct SnapshotPublisher {
    cell: Arc<ArcSwap<WorldSnapshot>>,
    min_interval: Duration,
    state: RefCell<PublishState>,
}

/// What the publisher remembers between publishes, so most of them allocate
/// nothing but the thread list.
#[derive(Debug)]
struct PublishState {
    last: Option<Instant>,
    generation: u64,
    layout: Arc<CityLayout>,
    layout_generation: u64,
    files: Arc<BTreeMap<LogicalPath, FileState>>,
    files_generation: u64,
}

impl SnapshotPublisher {
    /// Publishes the current world. Call **once per batch**, never per event.
    ///
    /// Returns `false` when the publish was skipped because
    /// [`SnapshotPublisher::min_interval`] had not elapsed — which is not an
    /// error, it is the rate limit doing its job. The next call publishes the
    /// newer state, so nothing is lost, only coalesced.
    pub fn publish(&self, world: &World) -> bool {
        self.publish_at(world, Instant::now())
    }

    /// [`SnapshotPublisher::publish`] against an explicit clock, for tests and
    /// for a render loop that already has a frame instant.
    pub fn publish_at(&self, world: &World, now: Instant) -> bool {
        if let Some(last) = self.state.borrow().last {
            if now.saturating_duration_since(last) < self.min_interval {
                return false;
            }
        }
        self.store(world, now);
        true
    }

    /// Publishes regardless of the rate limit.
    ///
    /// For the first frame and for shutdown, where "the operator is looking at
    /// stale state" costs more than a publish does.
    pub fn force(&self, world: &World) {
        self.store(world, Instant::now());
    }

    /// The floor between two publishes.
    pub fn min_interval(&self) -> Duration {
        self.min_interval
    }

    /// Sets the floor between two publishes. Takes `&mut self`, so it cannot be
    /// changed from behind a shared reference while the loop is running.
    pub fn set_min_interval(&mut self, interval: Duration) {
        self.min_interval = interval;
    }

    /// How many snapshots have been published.
    pub fn generation(&self) -> u64 {
        self.state.borrow().generation
    }

    fn store(&self, world: &World, now: Instant) {
        let (layout_gen, files_gen) = world.generations();
        let mut state = self.state.borrow_mut();
        if state.layout_generation != layout_gen {
            state.layout = Arc::new(world.layout.clone());
            state.layout_generation = layout_gen;
        }
        if state.files_generation != files_gen {
            state.files = Arc::new(world.files.clone());
            state.files_generation = files_gen;
        }
        state.generation = state.generation.wrapping_add(1);
        state.last = Some(now);

        let snapshot = WorldSnapshot {
            layout: Arc::clone(&state.layout),
            threads: world.threads_for_rail().into_iter().cloned().collect(),
            attention: world.attention.clone(),
            overlaps: world.overlaps.clone(),
            health: world.health.clone(),
            files: Arc::clone(&state.files),
            unattributed: world.unattributed.values().cloned().collect(),
            generation: state.generation,
            at: world.now(),
        };
        self.cell.store(Arc::new(snapshot));
    }
}

/// The reader half. Cloneable and cheap; the renderer loads it per frame.
///
/// One field and one method, on purpose — see the module docs.
#[derive(Debug, Clone)]
pub struct SnapshotReader {
    cell: Arc<ArcSwap<WorldSnapshot>>,
}

impl SnapshotReader {
    /// Loads the newest published snapshot. Never blocks and never allocates.
    pub fn load(&self) -> Arc<WorldSnapshot> {
        self.cell.load_full()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::World;
    use polis_events::SessionId;

    fn world() -> World {
        World::for_replay(CityLayout::default())
    }

    #[test]
    fn a_reader_sees_only_what_was_published() {
        let mut w = world();
        let (publisher, reader) = from_world(&w);
        let first = reader.load();
        assert!(first.threads.is_empty());

        w.thread_entry(&SessionId::new("s1"), Instant::now());
        assert!(
            reader.load().threads.is_empty(),
            "mutating the world must not reach the reader"
        );

        publisher.force(&w);
        assert_eq!(reader.load().threads.len(), 1);
        assert!(reader.load().generation > first.generation);
    }

    #[test]
    fn publishing_is_rate_limited_so_the_ingest_rate_cannot_drive_the_renderer() {
        let w = world();
        let (publisher, reader) = from_world(&w);
        // Past the floor the constructor's own publish set.
        let t0 = Instant::now() + Duration::from_secs(1);
        assert!(publisher.publish_at(&w, t0));
        let after_first = reader.load().generation;
        // 500 events/sec arriving in one millisecond must not publish 500 times.
        for i in 1..500 {
            let _ = publisher.publish_at(&w, t0 + Duration::from_micros(i));
        }
        assert_eq!(reader.load().generation, after_first);
        assert!(publisher.publish_at(&w, t0 + MIN_PUBLISH_INTERVAL));
        assert_eq!(reader.load().generation, after_first + 1);
    }

    #[test]
    fn the_layout_arc_is_reused_until_the_layout_changes() {
        // PRD §13: the base map is cached to a texture keyed on its identity, so
        // a snapshot that clones the city every frame invalidates it every frame.
        let mut w = world();
        let (publisher, reader) = from_world(&w);
        let first = reader.load();
        publisher.force(&w);
        let second = reader.load();
        assert!(
            Arc::ptr_eq(&first.layout, &second.layout),
            "an unchanged city must keep its identity"
        );
        w.set_layout(CityLayout::default());
        publisher.force(&w);
        assert!(!Arc::ptr_eq(&first.layout, &reader.load().layout));
    }

    #[test]
    fn the_files_arc_is_reused_until_a_file_changes() {
        let mut w = world();
        let (publisher, reader) = from_world(&w);
        publisher.force(&w);
        let a = reader.load();
        publisher.force(&w);
        assert!(Arc::ptr_eq(&a.files, &reader.load().files));
        w.file_entry(&LogicalPath::new("src/x.rs").unwrap()).reads = 1;
        publisher.force(&w);
        assert!(!Arc::ptr_eq(&a.files, &reader.load().files));
        assert_eq!(reader.load().files.len(), 1);
    }

    #[test]
    fn a_reader_is_shareable_and_a_publisher_is_not() {
        fn assert_send_sync<T: Send + Sync>() {}
        fn assert_send<T: Send>() {}
        assert_send_sync::<SnapshotReader>();
        assert_send_sync::<Arc<WorldSnapshot>>();
        assert_send::<SnapshotPublisher>();
        // `SnapshotPublisher: !Sync` is enforced by its `RefCell`; a test that
        // asserted it would have to name a negative bound, which stable Rust has
        // no syntax for. The `RefCell` field is the enforcement.
    }
}
