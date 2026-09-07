//! The linked filesystem view (PRD §12).
//!
//! > **Linked filesystem view** — a plain tree, co-equal with the map, not a
//! > fallback. Shared selection and highlight state; one keystroke swaps. Both
//! > are renderings of one data structure. The map is a lossy projection and the
//! > operator will need ground truth to check it against.
//!
//! Co-equal is taken literally: the tree occupies the same rectangle the map
//! does and `T` swaps them, rather than the tree being a drawer that opens
//! beside a map that keeps the real estate. Selection lives in
//! [`crate::mapview::ViewState`], which both views read and write, so selecting
//! `polis-world/src/apply.rs` in the tree and pressing `T` lands the camera on
//! its building.
//!
//! # The ground truth it exists to provide
//!
//! The map can only show a file that got a building. Files arrive from the
//! transcript that the layout has never heard of — created after the walk, in a
//! worktree, under a path the mapper could not resolve — and on the map they are
//! invisible or, worse, drawn at their parent district and mistaken for
//! something else. The tree lists them explicitly, marked `off-map`, because
//! *"the operator will need ground truth to check it against"* means the tree
//! has to be able to disagree with the map.
//!
//! # What co-equal costs
//!
//! Occupying the same rectangle is the cheap half. The rest is that everything
//! the operator can do on the map they can do here:
//!
//! * **The same selection.** [`crate::mapview::ViewState`] is the one copy, and
//!   `T` in either direction lands on the same file.
//! * **The same click.** A file row opens `$EDITOR`, exactly as its building
//!   does (PRD §12). A tree that only selects would make the two views two
//!   different products.
//! * **The same attention states.** All three of PRD §11.2 appear on the rows
//!   they belong to, and roll **up** to every ancestor directory — otherwise a
//!   collapsed tree would hide the one thing the product exists to show.
//! * **The same follow.** The thread the camera is bound to marks its files
//!   here too, so swapping views mid-follow does not lose the thread.

// One row of the tree is one ordered sequence of decisions — indent, arrow,
// attention mark, name, badges, follow marker, then the three ways it can be
// clicked — and splitting it into fragments each called once would put the
// order of a row in one function and its content in six others.
#![allow(clippy::too_many_lines)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use eframe::egui::{self, Color32, RichText};
use polis_events::LogicalPath;
use polis_world::attention::AttentionKind;
use polis_world::snapshot::WorldSnapshot;

use crate::drill;
use crate::mapview::ViewState;
use crate::palette;

/// Most visited paths rolled up for the follow highlight in one frame.
///
/// A thread that has touched more files than this is one whose highlight would
/// cover the tree anyway, and the roll-up is the only per-frame work here that
/// grows with session length.
const FOLLOW_ROLLUP_CAP: usize = 4_000;

/// What a frame of the tree asks the window to do.
///
/// The same two things a frame of the map asks for, which is the point: PRD §12
/// calls the two views co-equal, and a view that could only ever select would
/// not be.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TreeOut {
    /// A file row was clicked. Select it and open it in `$EDITOR` — the same
    /// handler a click on its building runs, so the two views cannot drift.
    pub clicked: Option<LogicalPath>,
    /// Where to cut the camera when the map comes back.
    pub centre_on: Option<LogicalPath>,
}

/// One node of the tree: a directory or a file.
#[derive(Debug, Default)]
struct Node {
    /// Children, directories and files together, in path order.
    children: BTreeMap<String, Node>,
    /// The logical path this node stands for.
    path: Option<LogicalPath>,
    /// Whether the layout gave it a building — the map/tree disagreement this
    /// view exists to surface.
    on_map: bool,
    /// Whether it is a leaf.
    file: bool,
    /// Files under here, for the directory summary line.
    files: usize,
}

/// The tree, built from the layout and the live file table.
///
/// Rebuilt only when either of those changes identity, which is what the `Arc`s
/// on [`WorldSnapshot`] are for.
#[derive(Debug, Default)]
pub struct TreeView {
    root: Node,
    /// Which directories are open.
    open: BTreeSet<String>,
    /// Identity of the layout and the file table the tree was built from.
    key: (usize, usize),
    /// Total files, and how many of them the map cannot draw.
    pub files: usize,
    /// Files with no building.
    pub off_map: usize,
    /// A path the caller should centre the camera on, consumed once.
    pending_reveal: Option<LogicalPath>,
}

impl TreeView {
    /// Rebuilds when the layout or the live file table changes identity.
    fn sync(&mut self, snapshot: &WorldSnapshot) {
        let key = (
            Arc::as_ptr(&snapshot.layout).cast::<()>() as usize,
            Arc::as_ptr(&snapshot.files).cast::<()>() as usize,
        );
        if key == self.key {
            return;
        }
        self.key = key;

        let mut root = Node::default();
        for path in snapshot.layout.buildings.keys() {
            insert(&mut root, path, true);
        }
        for path in snapshot.files.keys() {
            if snapshot.layout.buildings.contains_key(path) {
                continue;
            }
            insert(&mut root, path, false);
        }
        // Leaf-or-not is decided **after** everything is inserted, never as it
        // goes in. The live file table contains directories — a `Glob` over
        // `docs`, a `Bash` in `polis-events/src` — and marking the last
        // component a file on the way in turned every such directory into a
        // childless leaf, which on the live window silently deleted whole
        // subtrees from the one view that exists to be ground truth.
        let counts = if root.children.is_empty() {
            Counts::default()
        } else {
            count(&mut root)
        };
        self.root = root;
        self.files = counts.files;
        self.off_map = counts.off_map;

        // Open the first level, so the view is useful the moment it is swapped
        // in rather than a single collapsed row.
        if self.open.is_empty() {
            for name in self.root.children.keys() {
                self.open.insert(name.clone());
            }
        }
    }

    /// Opens every ancestor of a path and asks the caller to scroll to it.
    ///
    /// This is the map-to-tree half of the shared selection: clicking a building
    /// and pressing `T` lands on that file's row, expanded.
    pub fn reveal(&mut self, path: &LogicalPath) {
        let mut prefix = String::new();
        for component in path.components() {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            self.open.insert(prefix.clone());
        }
        self.pending_reveal = Some(path.clone());
    }

    /// Draws the tree.
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &WorldSnapshot,
        state: &mut ViewState,
    ) -> TreeOut {
        self.sync(snapshot);
        let reveal = self.pending_reveal.take();
        let mut out = TreeOut::default();
        let mut hovered = None;

        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!("{} files", self.files))
                    .color(palette::worker().color())
                    .monospace(),
            );
            if self.off_map > 0 {
                ui.label(
                    RichText::new(format!("· {} off-map", self.off_map))
                        .color(palette::needs_decision().color())
                        .monospace(),
                )
                .on_hover_text(
                    "Touched by an agent but absent from the layout: created after the walk, \
                     in another worktree, or under a path the mapper could not resolve. \
                     The map cannot draw these; this view is the ground truth.",
                );
            }
            // The two states the map would be showing right now, said in words,
            // so swapping views never loses context.
            if let Some(first) = snapshot.attention.first() {
                ui.label(
                    RichText::new(format!(
                        "· {} attention, worst {}",
                        snapshot.attention.len(),
                        drill::word(&first.kind)
                    ))
                    .color(drill::ink(&first.kind).color())
                    .small(),
                );
            }
            if let Some(id) = &state.follow {
                let label = snapshot.thread(id).map_or_else(
                    || drill::short_id(id.as_str()),
                    crate::mapview::thread_label,
                );
                ui.label(
                    RichText::new(format!("· following {label}"))
                        .color(palette::hover().color())
                        .small(),
                );
            }
            ui.label(
                RichText::new("· T swaps back to the map · click a file to open it")
                    .color(palette::status(polis_world::ThreadStatus::Idle).color())
                    .small(),
            );
        });
        ui.separator();

        let marks = Marks::of(snapshot);
        let following = Following::of(snapshot, state.follow.as_ref());
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut ctx = RowCtx {
                    snapshot,
                    state,
                    reveal: reveal.as_ref(),
                    marks: &marks,
                    following: &following,
                    out: &mut out,
                    hovered: &mut hovered,
                };
                let mut open = std::mem::take(&mut self.open);
                rows(ui, &self.root, "", 0, &mut open, &mut ctx);
                self.open = open;
            });

        state.hovered = hovered;
        out
    }
}

/// Where the three attention states of PRD §11.2 land in a filesystem tree, and
/// which directories carry one underneath them.
///
/// The roll-up is the load-bearing half. The map draws a mark at a point, and a
/// point is always visible somewhere on screen; a tree row inside a collapsed
/// directory is not visible at all. Without the roll-up the co-equal view would
/// be the one place in the window where contention can be entirely invisible.
#[derive(Debug, Default)]
struct Marks<'a> {
    /// Worst mark on each file.
    at: BTreeMap<LogicalPath, &'a AttentionKind>,
    /// Worst mark anywhere under each directory, keyed by the directory's
    /// logical path string — which is exactly the tree's own row key.
    under: BTreeMap<String, &'a AttentionKind>,
}

impl<'a> Marks<'a> {
    fn of(snapshot: &'a WorldSnapshot) -> Self {
        let mut marks = Self::default();
        for mark in &snapshot.attention {
            let Some(path) = drill::subject(snapshot, mark) else {
                continue;
            };
            let kind = &mark.kind;
            marks.keep_at(path.clone(), kind);
            // The walk starts at the path itself, not at its parent: a mark can
            // land on a directory — a shell call names its working directory —
            // and that directory's row is a `under` lookup, not an `at` one.
            let mut dir = Some(path);
            while let Some(current) = dir {
                if current.is_root() {
                    break;
                }
                marks.keep_under(current.as_str().to_owned(), kind);
                dir = current.parent();
            }
        }
        marks
    }

    fn keep_at(&mut self, path: LogicalPath, kind: &'a AttentionKind) {
        let slot = self.at.entry(path).or_insert(kind);
        if drill::rank(kind) < drill::rank(slot) {
            *slot = kind;
        }
    }

    fn keep_under(&mut self, key: String, kind: &'a AttentionKind) {
        let slot = self.under.entry(key).or_insert(kind);
        if drill::rank(kind) < drill::rank(slot) {
            *slot = kind;
        }
    }
}

/// The followed thread's footprint, as tree rows.
///
/// PRD §12 asks the two views to share highlight state, and on the map "the
/// followed thread" is a camera binding plus a cloud. In a tree it is this: the
/// files that thread has visited, and a count on every directory above them.
#[derive(Debug, Default)]
struct Following {
    files: BTreeSet<LogicalPath>,
    under: BTreeMap<String, usize>,
}

impl Following {
    fn of(snapshot: &WorldSnapshot, follow: Option<&polis_events::ThreadId>) -> Self {
        let mut out = Self::default();
        let Some(thread) = follow.and_then(|id| snapshot.thread(id)) else {
            return out;
        };
        for path in thread.visits.keys().take(FOLLOW_ROLLUP_CAP) {
            out.files.insert(path.clone());
            let mut dir = path.parent();
            while let Some(current) = dir {
                if current.is_root() {
                    break;
                }
                *out.under.entry(current.as_str().to_owned()).or_default() += 1;
                dir = current.parent();
            }
            // And the path itself, because a visit can name a directory.
            if !path.is_root() {
                *out.under.entry(path.as_str().to_owned()).or_default() += 1;
            }
        }
        out
    }

    fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

struct RowCtx<'a> {
    snapshot: &'a WorldSnapshot,
    state: &'a mut ViewState,
    reveal: Option<&'a LogicalPath>,
    marks: &'a Marks<'a>,
    following: &'a Following,
    out: &'a mut TreeOut,
    hovered: &'a mut Option<LogicalPath>,
}

/// One level of the tree. Directories first, then files, both in path order.
fn rows(
    ui: &mut egui::Ui,
    node: &Node,
    prefix: &str,
    depth: usize,
    open: &mut BTreeSet<String>,
    ctx: &mut RowCtx<'_>,
) {
    for (name, child) in &node.children {
        let key = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let is_open = open.contains(&key);
        let selected = child.file && ctx.state.selected.as_ref() == child.path.as_ref();

        // The same three states the map is drawing, on the row they belong to —
        // or, for a closed directory, the worst one anywhere inside it.
        let mark = if child.file {
            child.path.as_ref().and_then(|p| ctx.marks.at.get(p))
        } else {
            ctx.marks.under.get(&key)
        };
        let followed = if child.file {
            child
                .path
                .as_ref()
                .is_some_and(|p| ctx.following.files.contains(p))
        } else {
            ctx.following.under.contains_key(&key)
        };

        let response = ui
            .horizontal(|ui| {
                #[allow(clippy::cast_precision_loss)] // a nesting depth, not a measurement
                let indent = depth as f32 * 14.0;
                ui.add_space(indent);
                if child.file {
                    ui.add_space(12.0);
                } else {
                    let arrow = if is_open { "▾" } else { "▸" };
                    ui.label(
                        RichText::new(arrow)
                            .color(palette::status(polis_world::ThreadStatus::Idle).color())
                            .monospace(),
                    );
                }
                // Attention owns the top of the contrast range here as it does
                // on the map (PRD §10.3), so it is drawn before the name and in
                // shape as well as colour (PRD §11.4).
                if let Some(kind) = mark {
                    ui.label(
                        RichText::new(drill::tree_glyph(kind))
                            .monospace()
                            .strong()
                            .color(drill::ink(kind).color()),
                    )
                    .on_hover_text(drill::word(kind));
                }
                let mut text = RichText::new(name).monospace();
                text = if child.file {
                    text.color(if child.on_map {
                        palette::file_label().color()
                    } else {
                        palette::needs_decision().color()
                    })
                } else {
                    text.color(palette::district_label().color()).strong()
                };
                if selected {
                    text = text.background_color(Color32::from_rgb(40, 44, 52));
                }
                ui.label(text);
                match (child.file, &child.path) {
                    (true, Some(path)) => badges(ui, ctx.snapshot, path, child.on_map),
                    _ => {
                        ui.label(
                            RichText::new(format!("{}", child.files))
                                .small()
                                .color(palette::status(polis_world::ThreadStatus::Idle).color()),
                        );
                    }
                }
                if followed {
                    let count = if child.file {
                        None
                    } else {
                        ctx.following.under.get(&key).copied()
                    };
                    ui.label(
                        RichText::new(
                            count.map_or_else(|| "<- here".to_owned(), |n| format!("<- {n}")),
                        )
                        .small()
                        .color(palette::hover().color()),
                    )
                    .on_hover_text("The thread the camera is following has been here.");
                }
            })
            .response;

        // The whole row is the target, not just the text: a tree is a list of
        // rows and clicking the gap between two words must not miss.
        let row = response.rect;
        let hit = ui.interact(row, ui.id().with(&key), egui::Sense::click());
        if followed && !ctx.following.is_empty() {
            ui.painter()
                .rect_filled(row, 2.0, palette::hover().alpha(0.05));
        }
        if hit.hovered() {
            ui.painter()
                .rect_filled(row, 2.0, palette::hover().alpha(0.06));
            if let Some(path) = &child.path {
                *ctx.hovered = Some(path.clone());
            }
        }
        if hit.clicked() {
            if child.file {
                ctx.state.selected.clone_from(&child.path);
                ctx.out.centre_on.clone_from(&child.path);
                // PRD §12 gives a click one meaning, and this view is co-equal
                // with the one that already has it: *"Click a building → open in
                // `$EDITOR` […] Nothing more."*
                ctx.out.clicked.clone_from(&child.path);
            } else if is_open {
                open.remove(&key);
            } else {
                open.insert(key.clone());
            }
        }
        if ctx.reveal.is_some_and(|p| Some(p) == child.path.as_ref()) {
            hit.scroll_to_me(Some(egui::Align::Center));
        }

        if !child.file && open.contains(&key) {
            rows(ui, child, &key, depth + 1, open, ctx);
        }
    }
}

/// The live state of one file, exactly — PRD §12's *"Fuzzy above, exact
/// below"*, in its most literal form.
fn badges(ui: &mut egui::Ui, snapshot: &WorldSnapshot, path: &LogicalPath, on_map: bool) {
    if !on_map {
        ui.label(
            RichText::new("off-map")
                .small()
                .color(palette::needs_decision().color()),
        );
    }
    let Some(file) = snapshot.file(path) else {
        return;
    };
    if file.lines_added > 0 || file.lines_removed > 0 {
        ui.label(
            RichText::new(format!("+{} -{}", file.lines_added, file.lines_removed))
                .small()
                .color(palette::outcome(polis_events::Outcome::Done).color()),
        );
    } else if file.diff_lines > 0 {
        ui.label(
            RichText::new(format!("~{}", file.diff_lines))
                .small()
                .color(palette::outcome(polis_events::Outcome::Pending).color()),
        );
    }
    if !file.touched_by.is_empty() {
        ui.label(
            RichText::new(format!("[{}]", file.touched_by.len()))
                .small()
                .color(palette::trail().color()),
        );
    }
    if file.is_verified() {
        ui.label(
            RichText::new("ok")
                .small()
                .color(palette::done_verified().color()),
        );
    }
    if file.deleted {
        ui.label(
            RichText::new("deleted")
                .small()
                .color(palette::contention().color()),
        );
    }
}

fn insert(root: &mut Node, path: &LogicalPath, on_map: bool) {
    let components: Vec<&str> = path.components().collect();
    let mut node = root;
    for (i, component) in components.iter().enumerate() {
        let last = i + 1 == components.len();
        node = node.children.entry((*component).to_owned()).or_default();
        if last {
            node.on_map = on_map;
            node.path = Some(path.clone());
        }
    }
}

/// A subtree's leaf counts.
#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    /// Leaves under here.
    files: usize,
    /// Of which the map cannot draw.
    off_map: usize,
}

/// Decides which nodes are leaves and fills in each directory's file count,
/// bottom up.
///
/// A node is a file **iff it has no children**. Nothing else works: a path can
/// arrive from the live file table naming a directory the layout already filled
/// with buildings, and whichever of the two arrived last would otherwise decide
/// whether the whole subtree exists.
fn count(node: &mut Node) -> Counts {
    if node.children.is_empty() {
        node.file = true;
        node.files = 1;
        return Counts {
            files: 1,
            off_map: usize::from(!node.on_map),
        };
    }
    node.file = false;
    let mut total = Counts::default();
    for child in node.children.values_mut() {
        let sub = count(child);
        total.files += sub.files;
        total.off_map += sub.off_map;
    }
    node.files = total.files;
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_world::snapshot::WorldSnapshot;
    use std::collections::BTreeMap;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    /// A real layout, so the test is about the tree and not about a fixture:
    /// `buildings` are kept from the generated city and `touched` are added to
    /// the live file table whether or not they got one.
    fn snapshot_with(buildings: &[&str], touched: &[&str]) -> WorldSnapshot {
        let tree = polis_repo::synthetic::repository(40, 3);
        let mut layout =
            polis_layout::city::generate_with(&tree, &polis_layout::city::LayoutInputs::default())
                .layout;
        let template = layout
            .buildings
            .values()
            .next()
            .cloned()
            .expect("the synthetic city has buildings");
        layout.buildings.clear();
        for path in buildings {
            layout.buildings.insert(
                lp(path),
                polis_layout::Building {
                    path: lp(path),
                    ..template.clone()
                },
            );
        }
        let mut files = BTreeMap::new();
        for path in touched {
            files.insert(lp(path), polis_world::FileState::default());
        }
        let mut snapshot = WorldSnapshot::empty(Arc::new(layout));
        snapshot.files = Arc::new(files);
        snapshot
    }

    #[test]
    fn the_tree_is_the_layouts_files_as_a_hierarchy() {
        let snapshot = snapshot_with(&["src/a.rs", "src/inner/b.rs", "README.md"], &[]);
        let mut tree = TreeView::default();
        tree.sync(&snapshot);
        assert_eq!(tree.files, 3);
        assert_eq!(tree.off_map, 0);
        let src = tree.root.children.get("src").expect("src");
        assert_eq!(src.files, 2);
        assert!(src.children.contains_key("inner"));
        assert!(tree.root.children.get("README.md").expect("readme").file);
    }

    /// The reason the view is co-equal rather than a fallback: it can disagree
    /// with the map, and a file the map cannot draw has to be visible somewhere.
    #[test]
    fn a_touched_file_with_no_building_is_listed_and_marked_off_map() {
        let snapshot = snapshot_with(&["src/a.rs"], &["src/a.rs", "scratch/new.rs"]);
        let mut tree = TreeView::default();
        tree.sync(&snapshot);
        assert_eq!(tree.files, 2);
        assert_eq!(tree.off_map, 1, "scratch/new.rs has no building");
        let ghost = tree
            .root
            .children
            .get("scratch")
            .and_then(|d| d.children.get("new.rs"))
            .expect("the off-map file is in the tree");
        assert!(!ghost.on_map);
        let real = tree
            .root
            .children
            .get("src")
            .and_then(|d| d.children.get("a.rs"))
            .expect("the mapped file is in the tree");
        assert!(real.on_map);
    }

    /// Seen on the live window: whole subtrees vanished. The live file table
    /// contains **directories** — a `Glob` over `docs`, a shell call in
    /// `polis-events/src` — and marking the last component of every inserted
    /// path a file turned those directories into childless leaves stamped
    /// `off-map`. The one view that exists to be ground truth was deleting the
    /// ground truth.
    #[test]
    fn a_directory_that_is_also_in_the_file_table_keeps_its_children() {
        let snapshot = snapshot_with(
            &["polis-events/src/lib.rs", "polis-events/src/path.rs"],
            // The directory arrives after the two buildings, as a `Glob` on it
            // would.
            &["polis-events/src"],
        );
        let mut tree = TreeView::default();
        tree.sync(&snapshot);

        let src = tree
            .root
            .children
            .get("polis-events")
            .and_then(|d| d.children.get("src"))
            .expect("the directory is in the tree");
        assert!(!src.file, "it is a directory, whatever the file table says");
        assert_eq!(src.children.len(), 2, "and it kept both of its files");
        assert_eq!(src.files, 2);
        assert_eq!(tree.files, 2, "the count is leaves, not inserted paths");
        assert_eq!(tree.off_map, 0, "and a directory is not an off-map file");
    }

    #[test]
    fn revealing_a_path_opens_every_ancestor() {
        let snapshot = snapshot_with(&["a/b/c/d.rs"], &[]);
        let mut tree = TreeView::default();
        tree.sync(&snapshot);
        tree.open.clear();
        tree.reveal(&lp("a/b/c/d.rs"));
        assert!(tree.open.contains("a"));
        assert!(tree.open.contains("a/b"));
        assert!(tree.open.contains("a/b/c"));
        assert_eq!(tree.pending_reveal, Some(lp("a/b/c/d.rs")));
    }

    #[test]
    fn the_tree_is_rebuilt_only_when_the_data_changes_identity() {
        let snapshot = snapshot_with(&["src/a.rs"], &[]);
        let mut tree = TreeView::default();
        tree.sync(&snapshot);
        let key = tree.key;
        tree.sync(&snapshot);
        assert_eq!(tree.key, key);

        let other = snapshot_with(&["src/a.rs", "src/b.rs"], &[]);
        tree.sync(&other);
        assert_ne!(tree.key, key);
        assert_eq!(tree.files, 2);
    }
}
