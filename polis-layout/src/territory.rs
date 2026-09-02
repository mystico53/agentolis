//! The district tree — the directory hierarchy, weighed and ordered.
//!
//! # What this used to be, and why it is not that any more
//!
//! Until this commit the module was a recursive partition of the **plane**: a
//! wobbled nineteen-gon's convex hull fanned into nine wedges round the civic
//! square, then chord-split down the directory tree, with a plot allowed to
//! settle only inside its own district's polygon. It was grafted on to make
//! districts contiguous, and it did — at a price the three M1 gates all named
//! and none of them fixed:
//!
//! * the silhouette was the polygon, so **solidity was 0.995–0.999** on every
//!   corpus measured. A convex polygon is 1.00. The city read as a coin.
//! * a wedge boundary was an exactly straight road for its whole length, so
//!   there were **four to nine dead-straight district borders per repository**,
//!   each running from within 5 % of the radius out past 90 % of it, within two
//!   degrees of radial. The city read as a pie chart drawn on that coin.
//!
//! Neither was incidental. A partition of the plane into convex faces *is* a
//! diagram; no amount of wobble on the outer polygon or lean on the cuts makes
//! it a place.
//!
//! # What replaced it
//!
//! Contiguity is a property of a **graph**, not of a polygon, so it is now
//! obtained on the graph. [`crate::accrete`] grows the town with no territory
//! constraint at all — organically, on the frontier, exactly as
//! `docs/design/accretion` did — and [`crate::regions`] then partitions the
//! *plot adjacency graph* recursively down this same tree, in this same order,
//! by balanced weight. Every part of that partition is connected by
//! construction, so every district is one place on the map; and because the
//! partition never touches the plane, the outline stays the ragged outline of
//! the settled ground and no border is a straight line unless the ground made it
//! one.
//!
//! This module is what is left, and it is exactly what both of those stages
//! need: the directory tree, each node carrying its own and its subtree's
//! quantised weight, its age, and whether it is industrial, with children in
//! `(oldest file, path)` order.
//!
//! Weights are quantised to five significant binary digits, so adding one file
//! almost never moves a boundary (PRD §7.7).

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::collections::BTreeMap;

use polis_events::LogicalPath;

/// One directory in the district tree.
#[derive(Debug, Clone)]
pub(crate) struct Node {
    /// The directory this district is.
    pub(crate) path: LogicalPath,
    /// Index of the parent district, `None` for the repository root.
    pub(crate) parent: Option<u32>,
    /// Child districts, ordered by `(oldest file, path)`.
    pub(crate) children: Vec<u32>,
    /// Files directly in this directory, in quantised units.
    pub(crate) own_units: u64,
    /// `own_units` plus every descendant's.
    pub(crate) subtree_units: u64,
    /// Files directly in this directory, exactly.
    pub(crate) own_files: u32,
    /// `own_files` plus every descendant's.
    pub(crate) subtree_files: u32,
    /// Lowest growth index of a file **directly** in this directory.
    pub(crate) own_oldest: u32,
    /// Lowest growth index anywhere in the subtree — the district's own age.
    pub(crate) oldest: u32,
    /// True when the directory is `node_modules`, `vendor`, `target` and so on.
    pub(crate) industrial: bool,
}

/// The district tree.
#[derive(Debug, Clone, Default)]
pub(crate) struct Territory {
    /// Districts, in creation order (parents before children).
    pub(crate) nodes: Vec<Node>,
    /// Path to index.
    pub(crate) index: BTreeMap<LogicalPath, u32>,
}

impl Territory {
    /// The index of a district, if it has one.
    pub(crate) fn get(&self, path: &LogicalPath) -> Option<u32> {
        self.index.get(path).copied()
    }

    /// The repository root's index, which always exists once anything does.
    pub(crate) fn root(&self) -> u32 {
        self.index
            .get(&LogicalPath::root())
            .copied()
            .unwrap_or_default()
    }

    /// Depth of every district; the repository root is 0.
    ///
    /// Parents are always created before their children, so one forward pass is
    /// enough and there is no recursion to bound.
    pub(crate) fn depths(&self) -> Vec<u32> {
        let mut out = vec![0u32; self.nodes.len()];
        for (i, node) in self.nodes.iter().enumerate() {
            out[i] = node.parent.map_or(0, |p| out[p as usize] + 1);
        }
        out
    }
}

/// One district, as the tree weighs it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Demand {
    /// Growth index of the oldest file directly in the district.
    pub(crate) oldest: u32,
    /// How many files are directly in the district.
    pub(crate) files: u32,
}

/// Round up to five significant binary digits.
///
/// This is the stability knob. Two file counts in the same bucket produce the
/// same split, so adding one file to a district almost never moves a border,
/// and when it does the move is a single visible event rather than a permanent
/// tremor (PRD §7.7).
pub(crate) fn quantize_units(units: u64) -> u64 {
    if units <= 32 {
        return units;
    }
    let bits = 64 - units.leading_zeros();
    let shift = bits - 5;
    let step = 1u64 << shift;
    units.div_ceil(step) * step
}

/// Build the district tree from the growth sequence.
///
/// `districts` is `(logical path, demand)` for every directory that holds a
/// file; it is read in whatever order it arrives and sorted here, so the
/// caller's order cannot reach the tree (PRD §7.4).
pub(crate) fn build(
    districts: &[(LogicalPath, Demand)],
    industrial: &dyn Fn(&LogicalPath) -> bool,
) -> Territory {
    let mut t = Territory::default();
    if districts.is_empty() {
        return t;
    }
    let mut demand_of: BTreeMap<LogicalPath, (u32, u32)> = BTreeMap::new();
    for (path, d) in districts {
        let entry = demand_of.entry(path.clone()).or_insert((0, u32::MAX));
        entry.0 += d.files;
        entry.1 = entry.1.min(d.oldest);
    }

    // The root always exists: PRD §8's civic square is the repository root, and
    // both the growth and the partition need a single node to start from.
    ensure_one(&mut t, &LogicalPath::root(), industrial);
    for (district, (files, oldest)) in &demand_of {
        let id = ensure_one(&mut t, district, industrial);
        let node = &mut t.nodes[id as usize];
        node.own_files = *files;
        node.own_units = quantize_units(u64::from(*files)).max(1);
        node.own_oldest = node.own_oldest.min(*oldest);
        node.oldest = node.oldest.min(*oldest);
    }

    // Children in `(oldest file, path)` order, and subtree sums bottom-up.
    // Nodes are created parents-first, so one reverse pass is a full post-order.
    let order: Vec<u32> = (0..t.nodes.len() as u32).collect();
    for &i in order.iter().rev() {
        let mut kids = t.nodes[i as usize].children.clone();
        kids.sort_by_key(|&c| (t.nodes[c as usize].oldest, t.nodes[c as usize].path.clone()));
        let mut units = t.nodes[i as usize].own_units;
        let mut files = t.nodes[i as usize].own_files;
        let mut oldest = t.nodes[i as usize].oldest;
        for &c in &kids {
            units = units.saturating_add(t.nodes[c as usize].subtree_units);
            files = files.saturating_add(t.nodes[c as usize].subtree_files);
            oldest = oldest.min(t.nodes[c as usize].oldest);
        }
        let node = &mut t.nodes[i as usize];
        node.children = kids;
        node.subtree_units = units;
        node.subtree_files = files;
        node.oldest = oldest;
    }
    t
}

/// Create one node and every ancestor it needs, returning its index.
fn ensure_one(
    t: &mut Territory,
    path: &LogicalPath,
    industrial: &dyn Fn(&LogicalPath) -> bool,
) -> u32 {
    if let Some(&id) = t.index.get(path) {
        return id;
    }
    let parent = path.parent().map(|p| ensure_one(t, &p, industrial));
    let id = u32::try_from(t.nodes.len()).expect("district count fits in u32");
    t.nodes.push(Node {
        path: path.clone(),
        parent,
        children: Vec::new(),
        own_units: 0,
        subtree_units: 0,
        own_files: 0,
        subtree_files: 0,
        own_oldest: u32::MAX,
        oldest: u32::MAX,
        industrial: industrial(path),
    });
    t.index.insert(path.clone(), id);
    if let Some(p) = parent {
        t.nodes[p as usize].children.push(id);
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("valid path")
    }

    fn tree(items: &[(&str, u32, u32)]) -> Territory {
        let d: Vec<(LogicalPath, Demand)> = items
            .iter()
            .map(|(p, files, oldest)| {
                (
                    lp(p),
                    Demand {
                        oldest: *oldest,
                        files: *files,
                    },
                )
            })
            .collect();
        build(&d, &|_| false)
    }

    #[test]
    fn every_ancestor_of_a_district_is_a_district() {
        let t = tree(&[("a/b/c", 3, 7)]);
        for p in ["", "a", "a/b", "a/b/c"] {
            assert!(t.get(&lp(p)).is_some(), "{p} has no node");
        }
        let leaf = t.get(&lp("a/b/c")).expect("leaf");
        assert_eq!(t.nodes[leaf as usize].own_files, 3);
        let root = t.root();
        assert_eq!(t.nodes[root as usize].subtree_files, 3);
        assert_eq!(t.nodes[root as usize].oldest, 7);
    }

    #[test]
    fn children_are_ordered_by_age_then_path() {
        let t = tree(&[("z", 1, 1), ("a", 1, 5), ("m", 1, 1)]);
        let root = t.root();
        let names: Vec<String> = t.nodes[root as usize]
            .children
            .iter()
            .map(|&c| t.nodes[c as usize].path.as_str().to_owned())
            .collect();
        assert_eq!(names, vec!["m", "z", "a"]);
    }

    #[test]
    fn the_caller_order_cannot_reach_the_tree() {
        let a = tree(&[("a", 2, 1), ("b", 3, 2), ("c", 4, 3)]);
        let b = tree(&[("c", 4, 3), ("a", 2, 1), ("b", 3, 2)]);
        let names = |t: &Territory| -> Vec<String> {
            t.nodes.iter().map(|n| n.path.as_str().to_owned()).collect()
        };
        assert_eq!(names(&a), names(&b));
    }

    #[test]
    fn weights_are_quantised_so_one_more_file_rarely_moves_a_border() {
        assert_eq!(quantize_units(32), 32);
        assert_eq!(quantize_units(33), quantize_units(34));
        assert_eq!(quantize_units(1000), quantize_units(1001));
        assert!(quantize_units(1000) >= 1000);
    }

    #[test]
    fn depth_counts_directories_from_the_root() {
        let t = tree(&[("a/b/c", 1, 0)]);
        let d = t.depths();
        assert_eq!(d[t.root() as usize], 0);
        assert_eq!(d[t.get(&lp("a/b/c")).expect("leaf") as usize], 3);
    }
}
