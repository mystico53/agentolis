//! Measures how fast a district's **file names** actually drift, on real
//! repositories with real history — the evidence behind
//! `polis_repo::llm::cache::DEFAULT_DRIFT_THRESHOLD_PERMILLE`.
//!
//! ```text
//! cargo run --release -p polis-repo --example llm_drift -- <repo>... [--days 30,90,365]
//! ```
//!
//! # What it does, and why it can be trusted
//!
//! For each repository and each look-back window it lists the tracked files at
//! `HEAD` and at the last commit before *N* days ago, using
//! `git ls-tree -r --name-only <rev>`. **Nothing is checked out and nothing is
//! written**: the working tree is not touched, so this can be run against a
//! repository somebody is working in.
//!
//! It then partitions both file lists with the *same*
//! [`polis_repo::neighborhoods::Neighborhoods`] code the map uses, and for every
//! district present at both ends reports the Jaccard distance between its
//! file-name sets.
//!
//! Districts that appear or vanish are counted separately, because those are
//! already handled — a new district is `Missing` and a split or merge is caught
//! structurally by the child-set comparison, without any threshold at all.
//!
//! # Reading the output
//!
//! The number that matters is the **fires** column: how many surviving districts
//! would be re-described at each candidate threshold over that window. A
//! threshold that fires on most districts every month turns a one-time cost into
//! a per-commit one and makes the map's text churn; one that never fires lets a
//! district quietly become something else while its caption insists otherwise.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use polis_events::LogicalPath;
use polis_repo::llm::cache::{Sketch, DEFAULT_DRIFT_THRESHOLD_PERMILLE};
use polis_repo::neighborhoods::{NeighborhoodOptions, Neighborhoods};
use polis_repo::{FileMeta, RepoTree};

/// Thresholds compared side by side, in parts per thousand.
const CANDIDATES: &[u16] = &[250, 330, 440, 500, 660];

// Flag parsing and a report, in one flow. A tool's `main` reads better as one
// sequence than as six helpers that each run once.
#[allow(clippy::too_many_lines)]
fn main() {
    let mut repos: Vec<PathBuf> = Vec::new();
    let mut windows: Vec<Window> = vec![Window::Days(30), Window::Days(90), Window::Days(365)];
    let mut args = std::env::args().skip(1);
    let mut replaced = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--days" | "--commits" => {
                let commits = arg == "--commits";
                if !replaced {
                    windows.clear();
                    replaced = true;
                }
                windows.extend(
                    args.next()
                        .unwrap_or_default()
                        .split(',')
                        .filter_map(|d| d.trim().parse::<u32>().ok())
                        .map(|n| {
                            if commits {
                                Window::Commits(n)
                            } else {
                                Window::Days(n)
                            }
                        }),
                );
            }
            other => repos.push(PathBuf::from(other)),
        }
    }
    if repos.is_empty() {
        eprintln!("usage: llm_drift <repo>... [--days 30,90,365] [--commits 25,100,400]");
        std::process::exit(2);
    }

    println!(
        "| repository | window | districts | survived | appeared | vanished | median ‰ | p90 ‰ | {} |",
        CANDIDATES
            .iter()
            .map(|t| format!("fires ≥{t}"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    println!(
        "|---|---:|---:|---:|---:|---:|---:|---:|{}",
        "---:|".repeat(CANDIDATES.len())
    );

    let mut totals: BTreeMap<(Window, u16), (u32, u32)> = BTreeMap::new();
    for repo in &repos {
        let name = repo.file_name().map_or_else(
            || repo.display().to_string(),
            |n| n.to_string_lossy().into(),
        );
        for window in &windows {
            let label = window.label();
            let Some(row) = measure(repo, *window) else {
                println!(
                    "| {name} | {label} | — | — | — | — | — | — |{}",
                    " — |".repeat(CANDIDATES.len())
                );
                continue;
            };
            let mut fires = Vec::new();
            for threshold in CANDIDATES {
                let n = row.drifts.iter().filter(|d| **d >= *threshold).count();
                let entry = totals.entry((*window, *threshold)).or_insert((0, 0));
                entry.0 += u32::try_from(n).unwrap_or(u32::MAX);
                entry.1 += u32::try_from(row.drifts.len()).unwrap_or(u32::MAX);
                fires.push(format!("{n}"));
            }
            println!(
                "| {name} | {label} | {} | {} | {} | {} | {} | {} | {} |",
                row.districts,
                row.drifts.len(),
                row.appeared,
                row.vanished,
                percentile(&row.drifts, 50),
                percentile(&row.drifts, 90),
                fires.join(" | ")
            );
        }
    }

    println!("\n### Totals across every repository\n");
    println!(
        "| window | surviving districts | {} |",
        CANDIDATES
            .iter()
            .map(|t| format!("≥{t} ‰"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    println!("|---:|---:|{}", "---:|".repeat(CANDIDATES.len()));
    for window in &windows {
        let survived = totals
            .get(&(*window, CANDIDATES[0]))
            .map_or(0, |(_, total)| *total);
        let cells: Vec<String> = CANDIDATES
            .iter()
            .map(|t| {
                let (fires, total) = totals.get(&(*window, *t)).copied().unwrap_or((0, 0));
                let share = if total == 0 {
                    0.0
                } else {
                    f64::from(fires) * 100.0 / f64::from(total)
                };
                format!("{fires} ({share:.0} %)")
            })
            .collect();
        println!(
            "| {} | {survived} | {} |",
            window.label(),
            cells.join(" | ")
        );
    }
    println!("\nShipped threshold: {DEFAULT_DRIFT_THRESHOLD_PERMILLE} ‰.");
}

/// How far back to look.
///
/// Both are needed. A calendar window answers "how often would this fire in
/// normal use", which is the operational question — and on a repository nobody
/// has touched for a month it correctly answers "never". A commit window
/// answers "how much does a district move per unit of *work*", which is the
/// design question, and it keeps a dormant repository from silently dropping
/// out of the sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Window {
    Days(u32),
    Commits(u32),
}

impl Window {
    fn label(self) -> String {
        match self {
            Self::Days(n) => format!("{n}d"),
            Self::Commits(n) => format!("{n}c"),
        }
    }
}

/// One repository, one window.
struct Row {
    districts: usize,
    appeared: usize,
    vanished: usize,
    drifts: Vec<u16>,
}

fn measure(repo: &Path, window: Window) -> Option<Row> {
    let head = ls_tree(repo, "HEAD")?;
    let then_rev = match window {
        Window::Days(days) => {
            let spec = format!("--before={days} days ago");
            git(repo, &["rev-list", "-1", &spec, "HEAD"])?
        }
        Window::Commits(n) => {
            // `HEAD~n` fails on a shorter history; fall back to the root commit.
            let spec = format!("HEAD~{n}");
            git(
                repo,
                &["rev-parse", "--verify", &format!("{spec}^{{commit}}")],
            )
            .or_else(|| git(repo, &["rev-list", "--max-parents=0", "-1", "HEAD"]))?
        }
    };
    let then_rev = then_rev.trim();
    if then_rev.is_empty() {
        return None;
    }
    let then = ls_tree(repo, then_rev)?;
    if head.is_empty() || then.is_empty() {
        return None;
    }

    let options = NeighborhoodOptions::default();
    let now_hoods = Neighborhoods::build(&tree_of(&head), &options);
    let then_hoods = Neighborhoods::build(&tree_of(&then), &options);
    let now_names = names_of(&now_hoods, &head);
    let then_names = names_of(&then_hoods, &then);

    let now_paths: BTreeSet<&LogicalPath> = now_names.keys().collect();
    let then_paths: BTreeSet<&LogicalPath> = then_names.keys().collect();
    let mut drifts = Vec::new();
    for path in now_paths.intersection(&then_paths) {
        let a = Sketch::build(then_names.get(*path).expect("present"));
        let b = Sketch::build(now_names.get(*path).expect("present"));
        drifts.push(a.distance_permille(&b));
    }
    drifts.sort_unstable();
    Some(Row {
        districts: now_paths.len(),
        appeared: now_paths.difference(&then_paths).count(),
        vanished: then_paths.difference(&now_paths).count(),
        drifts,
    })
}

/// A district's own file names, relative to it.
fn names_of(hoods: &Neighborhoods, files: &[LogicalPath]) -> BTreeMap<LogicalPath, Vec<String>> {
    let mut out: BTreeMap<LogicalPath, Vec<String>> = BTreeMap::new();
    for hood in hoods.all() {
        if hood.is_industrial() {
            // Never described, so never measured.
            continue;
        }
        out.entry(hood.path.clone()).or_default();
    }
    for path in files {
        let Some(owner) = hoods.district_of(path) else {
            continue;
        };
        if owner.is_industrial() {
            continue;
        }
        let relative = if owner.path.is_root() {
            path.as_str()
        } else {
            path.as_str()
                .get(owner.path.as_str().len() + 1..)
                .unwrap_or(path.as_str())
        };
        out.entry(owner.path.clone())
            .or_default()
            .push(relative.to_owned());
    }
    out
}

fn tree_of(files: &[LogicalPath]) -> RepoTree {
    let mut tree = RepoTree::default();
    for path in files {
        tree.files
            .insert(path.clone(), FileMeta::untracked(path.clone(), 1));
    }
    tree
}

fn ls_tree(repo: &Path, rev: &str) -> Option<Vec<LogicalPath>> {
    let out = git(repo, &["ls-tree", "-r", "--name-only", rev])?;
    Some(
        out.lines()
            .filter_map(|l| LogicalPath::new(l).ok())
            .collect(),
    )
}

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn percentile(sorted: &[u16], p: usize) -> String {
    if sorted.is_empty() {
        return "—".to_owned();
    }
    let index = (sorted.len() * p / 100).min(sorted.len() - 1);
    sorted[index].to_string()
}
