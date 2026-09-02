//! A synthetic 5000-file repository with a plausible directory shape and a
//! plausible three-year history: a few packages that were there from the start
//! and grew huge, a long tail of small directories added late, deep nesting in
//! the parts that got refactored.

use polis_events::LogicalPath;
use polis_layout::determinism::SeededRng;

use crate::accrete::FileRec;
use crate::repo::RepoData;

const TOP: &[(&str, f64, f64)] = &[
    // (name, share of the repo, how early it was born in [0,1])
    ("core", 0.11, 0.00),
    ("server", 0.13, 0.04),
    ("web", 0.19, 0.10),
    ("mobile", 0.07, 0.46),
    ("services", 0.14, 0.22),
    ("packages", 0.09, 0.30),
    ("infra", 0.04, 0.18),
    ("tools", 0.03, 0.55),
    ("docs", 0.03, 0.12),
    ("tests", 0.06, 0.08),
    ("vendor", 0.08, 0.02),
    ("scripts", 0.02, 0.62),
    ("examples", 0.01, 0.72),
];

const MID: &[&str] = &[
    "src", "lib", "api", "model", "view", "store", "auth", "billing", "search", "graph", "queue",
    "cache", "codec", "render", "parser", "runtime", "config", "util", "net", "db", "schema",
    "worker", "stream", "media", "ui", "hooks", "pages", "components", "handlers", "adapters",
    "domain", "ports",
];

const LEAF: &[&str] = &[
    "session", "token", "user", "account", "invoice", "ledger", "index", "query", "plan", "shard",
    "router", "server", "client", "codec", "frame", "buffer", "pool", "retry", "limit", "clock",
    "trace", "metric", "event", "state", "reducer", "widget", "panel", "modal", "form", "table",
    "chart", "theme", "layout", "sprite", "loader", "mapper", "policy", "guard", "hasher", "signer",
];

const EXT: &[&str] = &["rs", "ts", "tsx", "py", "go", "md", "json", "sql"];

struct Dir {
    path: String,
    birth: f64,
    files: u32,
    depth: u32,
}

pub fn generate(total: usize, seed: u64) -> RepoData {
    let mut rng = SeededRng::for_seed(seed, "synth.repo");
    let mut dirs: Vec<Dir> = Vec::new();

    // Root-level config files: the civic square.
    dirs.push(Dir {
        path: String::new(),
        birth: 0.0,
        files: 9,
        depth: 0,
    });

    for (name, share, born) in TOP {
        let target = (total as f64 * share).round() as u32;
        // Grow a subtree until it holds `target` files.
        let mut frontier: Vec<(String, f64, u32)> = vec![((*name).to_string(), *born, 1)];
        let mut placed = 0u32;
        let mut guard = 0;
        while placed < target && guard < 20_000 {
            guard += 1;
            let pick = (rng.next_f64().powf(1.5) * frontier.len() as f64) as usize;
            let pick = pick.min(frontier.len() - 1);
            let (parent, pbirth, depth) = frontier[pick].clone();

            // How many files live directly here: heavy tail.
            let u = rng.next_f64();
            let here = if u < 0.55 {
                1 + (rng.next_f64() * 5.0) as u32
            } else if u < 0.9 {
                4 + (rng.next_f64() * 16.0) as u32
            } else {
                20 + (rng.next_f64() * 70.0) as u32
            };
            let here = here.min(target.saturating_sub(placed)).max(1);
            let jitter = rng.next_f64() * 0.20;
            let birth = (pbirth + jitter * (1.0 - pbirth)).clamp(0.0, 1.0);
            dirs.push(Dir {
                path: parent.clone(),
                birth,
                files: here,
                depth,
            });
            placed += here;

            // Spawn children; shallower directories branch more.
            if depth < 6 {
                let kids = if depth <= 1 {
                    2 + (rng.next_f64() * 4.0) as u32
                } else if depth <= 3 {
                    1 + (rng.next_f64() * 3.0) as u32
                } else {
                    (rng.next_f64() * 2.0) as u32
                };
                for _ in 0..kids {
                    let pool = if depth <= 1 { MID } else { LEAF };
                    let n = pool[(rng.next_f64() * pool.len() as f64) as usize % pool.len()];
                    let cand = format!("{parent}/{n}");
                    if frontier.iter().any(|(p, _, _)| *p == cand) {
                        continue;
                    }
                    let cb = (birth + rng.next_f64() * 0.30 * (1.0 - birth)).clamp(0.0, 1.0);
                    frontier.push((cand, cb, depth + 1));
                }
            }
            frontier.remove(pick);
            if frontier.is_empty() {
                frontier.push((parent, pbirth, depth));
            }
        }
    }

    // Materialise files with birth times, then rank to get the growth order.
    let t0: i64 = 1_650_000_000;
    let span: i64 = 3 * 365 * 24 * 3600;
    let mut raw: Vec<(f64, String, u64)> = Vec::new();
    for d in &dirs {
        for i in 0..d.files {
            let pool = if d.depth == 0 { LEAF } else { LEAF };
            let n = pool[(rng.next_f64() * pool.len() as f64) as usize % pool.len()];
            let e = EXT[(rng.next_f64() * EXT.len() as f64) as usize % EXT.len()];
            let name = if d.path.is_empty() {
                format!("{n}{i}.{e}")
            } else {
                format!("{}/{}{}.{}", d.path, n, i, e)
            };
            // Files appear over a window that opens at the directory's birth.
            let spread = 0.06 + rng.next_f64() * 0.35;
            let t = (d.birth + rng.next_f64().powf(1.6) * spread).clamp(0.0, 1.0);
            let u = rng.next_f64();
            let size = if u < 0.6 {
                300 + (rng.next_f64() * 4_000.0) as u64
            } else if u < 0.95 {
                4_000 + (rng.next_f64() * 30_000.0) as u64
            } else {
                30_000 + (rng.next_f64() * 220_000.0) as u64
            };
            raw.push((t, name, size));
        }
    }
    raw.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    raw.truncate(total);

    let mut files: Vec<FileRec> = Vec::new();
    for (i, (t, name, size)) in raw.iter().enumerate() {
        let Ok(lp) = LogicalPath::new(name) else {
            continue;
        };
        let industrial = name.starts_with("vendor/");
        let stem = name.rsplit('/').next().unwrap_or(name);
        files.push(FileRec {
            path: lp,
            size_bytes: *size,
            growth_index: u32::try_from(i).unwrap_or(u32::MAX),
            added_at: t0 + (t * span as f64) as i64,
            last_touched: t0 + (t * span as f64) as i64,
            industrial,
            monument: stem.starts_with("index") || stem.starts_with("server0") || i % 640 == 0,
        });
    }

    // Imports: mostly local, a minority crossing districts, with a few hubs.
    let n = files.len() as u32;
    let mut imports: Vec<(u32, u32)> = Vec::new();
    let hubs: Vec<u32> = (0..n).filter(|i| i % 271 == 0).collect();
    for i in 0..n {
        let k = (rng.next_f64() * 4.0) as u32;
        for _ in 0..k {
            let t = if rng.next_f64() < 0.30 {
                if !hubs.is_empty() && rng.next_f64() < 0.5 {
                    hubs[(rng.next_f64() * hubs.len() as f64) as usize % hubs.len()]
                } else {
                    (rng.next_f64() * f64::from(n)) as u32 % n
                }
            } else {
                // Near neighbour in the growth order: same area of the tree.
                let d = ((rng.next_f64() - 0.5) * 60.0) as i64;
                ((i as i64 + d).rem_euclid(i64::from(n))) as u32
            };
            if t != i {
                imports.push((i, t));
            }
        }
    }
    imports.sort_unstable();
    imports.dedup();
    RepoData { files, imports }
}
