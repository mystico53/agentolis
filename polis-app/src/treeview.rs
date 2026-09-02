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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use eframe::egui::{self, Color32, RichText};
use polis_events::LogicalPath;
use polis_world::snapshot::WorldSnapshot;

use crate::mapview::ViewState;
use crate::palette;

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
        let mut files = 0;
        let mut off_map = 0;
        for path in snapshot.layout.buildings.keys() {
            insert(&mut root, path, true);
            files += 1;
        }
        for path in snapshot.files.keys() {
            if snapshot.layout.buildings.contains_key(path) {
                continue;
            }
            insert(&mut root, path, false);
            files += 1;
            off_map += 1;
        }
        count(&mut root);
        self.root = root;
        self.files = files;
        self.off_map = off_map;

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

    /// Draws the tree. Returns a path to centre the camera on, if a row asked
    /// for it.
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &WorldSnapshot,
        state: &mut ViewState,
    ) -> Option<LogicalPath> {
        self.sync(snapshot);
        let reveal = self.pending_reveal.take();
        let mut centre_on = None;
        let mut hovered = None;

        ui.horizontal(|ui| {
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
            ui.label(
                RichText::new("· T swaps back to the map")
                    .color(palette::status(polis_world::ThreadStatus::Idle).color())
                    .small(),
            );
        });
        ui.separator();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut ctx = RowCtx {
                    snapshot,
                    state,
                    reveal: reveal.as_ref(),
                    centre_on: &mut centre_on,
                    hovered: &mut hovered,
                };
                let mut open = std::mem::take(&mut self.open);
                rows(ui, &self.root, "", 0, &mut open, &mut ctx);
                self.open = open;
            });

        if let Some(path) = hovered {
            state.hovered = Some(path);
        }
        centre_on
    }
}

struct RowCtx<'a> {
    snapshot: &'a WorldSnapshot,
    state: &'a mut ViewState,
    reveal: Option<&'a LogicalPath>,
    centre_on: &'a mut Option<LogicalPath>,
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
        let selected = ctx.state.selected.as_ref() == child.path.as_ref();

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
                if let Some(path) = &child.path {
                    badges(ui, ctx.snapshot, path, child.on_map);
                } else {
                    ui.label(
                        RichText::new(format!("{}", child.files))
                            .small()
                            .color(palette::status(polis_world::ThreadStatus::Idle).color()),
                    );
                }
            })
            .response;

        // The whole row is the target, not just the text: a tree is a list of
        // rows and clicking the gap between two words must not miss.
        let row = response.rect;
        let hit = ui.interact(row, ui.id().with(&key), egui::Sense::click());
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
                ctx.centre_on.clone_from(&child.path);
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
            RichText::new(format!("{}◆", file.touched_by.len()))
                .small()
                .color(palette::trail().color()),
        );
    }
    if file.is_verified() {
        ui.label(
            RichText::new("✓")
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
            node.file = true;
            node.on_map = on_map;
            node.path = Some(path.clone());
        }
    }
}

/// Fills in each directory's file count, bottom up.
fn count(node: &mut Node) -> usize {
    if node.file {
        return 1;
    }
    let mut total = 0;
    for child in node.children.values_mut() {
        total += count(child);
    }
    node.files = total;
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
