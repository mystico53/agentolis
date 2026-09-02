//! A synthetic 5000-file repository with realistic shape: deep nesting, a few
//! very large directories, a long tail of small ones, and a git history in
//! which old trees were committed first.

use polis_layout::determinism as det;

use crate::city::{FileRec, Repo};

const NOUNS: [&str; 64] = [
    "auth", "core", "net", "store", "cache", "index", "parse", "render", "shader", "mesh", "audio",
    "input", "codec", "proto", "sync", "queue", "pool", "alloc", "trace", "metric", "log", "cli",
    "http", "grpc", "sql", "orm", "graph", "tree", "table", "field", "widget", "layout", "theme",
    "icon", "route", "guard", "token", "hash", "crypt", "sign", "diff", "patch", "merge", "commit",
    "branch", "remote", "pack", "delta", "stream", "buffer", "frame", "clock", "timer", "task",
    "actor", "chan", "lock", "atom", "slab", "arena", "vec", "map", "set", "iter",
];
const EXTS: [&str; 6] = ["rs", "ts", "tsx", "py", "js", "go"];

struct Gen {
    rng: det::SeededRng,
    files: Vec<(String, u64, u32)>,
    dirs: Vec<(String, u32)>,
}

impl Gen {
    fn word(&mut self, salt: u64) -> String {
        let a = NOUNS[(self.rng.next_u64() ^ salt) as usize % NOUNS.len()];
        if self.rng.next_u64() % 3 == 0 {
            let b = NOUNS[self.rng.next_u64() as usize % NOUNS.len()];
            format!("{a}_{b}")
        } else {
            a.to_string()
        }
    }

    fn build(&mut self, prefix: &str, target: usize, depth: u32, epoch: u32) {
        self.dirs.push((prefix.to_string(), epoch));
        let leaf_cap = 5 + (self.rng.next_u64() % 11) as usize;
        if target <= leaf_cap || depth == 0 {
            self.emit(prefix, target, epoch);
            return;
        }
        // 12-30% of the files sit directly in this directory.
        let direct = ((target as f64) * self.rng.range_f64(0.10, 0.28)) as usize;
        let direct = direct.min(target.saturating_sub(2)).min(26);
        self.emit(prefix, direct, epoch);
        let rest = target - direct;
        let k = (2 + (self.rng.next_u64() % 5) as usize).min(rest.max(1));
        // Uneven split: one child often takes most of the mass.
        let mut cuts: Vec<f64> = (0..k).map(|_| self.rng.range_f64(0.06, 1.0).powf(1.7)).collect();
        let s: f64 = cuts.iter().sum();
        for c in &mut cuts {
            *c /= s;
        }
        let mut left = rest;
        for (i, frac) in cuts.iter().enumerate() {
            let n = if i == k - 1 {
                left
            } else {
                ((rest as f64 * frac).round() as usize).min(left)
            };
            left -= n;
            if n == 0 {
                continue;
            }
            let name = self.word(i as u64 * 977);
            let child = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let de = epoch + 1 + (self.rng.next_u64() % 4) as u32;
            self.build(&child, n, depth - 1, de);
        }
    }

    fn emit(&mut self, prefix: &str, n: usize, epoch: u32) {
        for i in 0..n {
            let stem = self.word(i as u64 * 31);
            let ext = EXTS[self.rng.next_u64() as usize % EXTS.len()];
            let name = format!("{stem}_{i:02}.{ext}");
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            // Log-normal-ish sizes.
            let s = (self.rng.range_f64(0.0, 1.0).powf(3.0) * 46_000.0 + 220.0) as u64;
            self.files.push((path, s, epoch));
        }
    }
}

pub fn synth() -> Repo {
    let mut g = Gen {
        rng: det::SeededRng::for_seed(0x504F_4C49_5300_5359, "synth-repo"),
        files: Vec::new(),
        dirs: Vec::new(),
    };
    // (top-level name, files, max depth, birth epoch). Uneven on purpose:
    // one enormous vendored tree, two large product trees, a long tail.
    let plan: [(&str, usize, u32, u32); 12] = [
        ("", 18, 0, 0),
        ("src", 1180, 5, 1),
        ("vendor", 1420, 6, 2),
        ("packages", 880, 5, 6),
        ("tests", 470, 4, 4),
        ("web", 410, 4, 9),
        ("docs", 260, 3, 3),
        ("tools", 150, 3, 12),
        ("scripts", 80, 2, 14),
        ("bench", 60, 2, 16),
        ("examples", 45, 2, 18),
        ("migrations", 30, 1, 11),
    ];
    for (name, n, d, e) in plan {
        g.build(name, n, d, e);
    }
    // A long tail of tiny top-level directories.
    for i in 0..22 {
        let name = format!("pkg_{i:02}");
        let n = 2 + (g.rng.next_u64() % 7) as usize;
        let e = 8 + (g.rng.next_u64() % 14) as u32;
        g.build(&name, n, 1, e);
    }

    // Growth order: everything an older tree committed comes first. Within one
    // epoch, order by path so the sequence is canonical.
    let mut idx: Vec<usize> = (0..g.files.len()).collect();
    idx.sort_by(|&a, &b| {
        g.files[a]
            .2
            .cmp(&g.files[b].2)
            .then(g.files[a].0.cmp(&g.files[b].0))
    });
    let mut growth = vec![0u32; g.files.len()];
    for (rank, &i) in idx.iter().enumerate() {
        growth[i] = u32::try_from(rank).unwrap();
    }

    let files: Vec<FileRec> = g
        .files
        .iter()
        .enumerate()
        .map(|(i, (p, s, _))| FileRec {
            path: p.clone(),
            size: *s,
            growth: growth[i],
        })
        .collect();

    // Cross-district imports, biased toward tree-near districts.
    let mut dirs: Vec<String> = g.dirs.iter().map(|(d, _)| d.clone()).collect();
    dirs.sort();
    dirs.dedup();
    let mut rng = det::SeededRng::for_seed(0x494D_504F_5254_0001, "synth-imports");
    let mut streets: Vec<(String, String, u32)> = Vec::new();
    for (i, from) in dirs.iter().enumerate() {
        let k = rng.next_u64() % 4;
        for _ in 0..k {
            let mut j = (i as i64 + (rng.next_u64() % 21) as i64 - 10).rem_euclid(dirs.len() as i64)
                as usize;
            if rng.next_u64() % 6 == 0 {
                j = rng.below(dirs.len() as u64) as usize;
            }
            if j == i {
                continue;
            }
            let n = 1 + (rng.range_f64(0.0, 1.0).powf(2.4) * 60.0) as u32;
            streets.push((from.clone(), dirs[j].clone(), n));
        }
    }
    streets.sort();
    Repo { files, streets }
}
