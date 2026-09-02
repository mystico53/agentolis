//! A synthetic 5000-file repository with a realistic directory shape and a
//! realistic git history: deep nesting, a few very large trees, a long tail of
//! small ones, and whole directories that appear together in time.

use std::collections::BTreeMap;

use polis_events::{LogicalPath, WallTime};
use polis_layout::determinism::SeededRng;
use polis_repo::{FileClass, FileMeta, Language, RepoTree};

const DIRS: &[&str] = &[
    "core", "api", "auth", "model", "view", "store", "util", "net", "sync", "cache", "parse",
    "render", "engine", "runtime", "codec", "proto", "query", "index", "graph", "schema", "shared",
    "common", "internal", "adapters", "handlers", "workers", "jobs", "events", "session", "client",
    "server", "compiler", "lexer", "types", "traits", "layout", "widgets", "hooks", "state",
    "router", "billing", "search", "audit", "telemetry", "crypto", "queue", "stream", "pipeline",
    "fixtures", "helpers",
];

const STEMS: &[&str] = &[
    "mod", "index", "main", "handler", "service", "client", "server", "config", "types", "utils",
    "helpers", "worker", "parser", "builder", "writer", "reader", "adapter", "registry", "context",
    "session", "record", "event", "state", "store", "query", "cache", "codec", "router", "guard",
    "policy", "schema", "loader", "walker", "visitor", "printer", "encoder", "decoder", "resolver",
    "manager", "factory", "bridge", "shim", "pool", "buffer", "queue", "task", "job", "metrics",
    "tracing", "errors",
];

const EXTS: &[&str] = &["rs", "ts", "tsx", "js", "py", "md", "json", "toml", "sql", "yaml"];

fn lang(ext: &str) -> Option<Language> {
    match ext {
        "rs" => Some(Language::Rust),
        "ts" => Some(Language::TypeScript),
        "tsx" => Some(Language::Tsx),
        "js" => Some(Language::JavaScript),
        "py" => Some(Language::Python),
        _ => None,
    }
}

struct Raw {
    path: LogicalPath,
    size: u64,
    founded: i64,
}

#[allow(clippy::too_many_arguments)]
fn gen_dir(prefix: &str, budget: usize, depth: usize, founded: i64, out: &mut Vec<Raw>) {
    let p = LogicalPath::new(prefix).unwrap();
    let mut rng = SeededRng::for_path(&p, "synth-dir");
    if budget == 0 {
        return;
    }
    if budget <= 13 || depth >= 5 {
        emit_files(prefix, budget, founded, out);
        return;
    }
    // a handful of files sit directly in this directory
    let direct = (rng.next_f64() * 5.0) as usize;
    let direct = direct.min(budget.saturating_sub(2));
    if direct > 0 {
        emit_files(prefix, direct, founded, out);
    }
    let mut left = budget - direct;
    let k = 2 + (rng.next_f64() * rng.next_f64() * 6.0) as usize;
    // skewed weights: one child usually dominates, giving very uneven sizes
    let mut w: Vec<f64> = (0..k).map(|_| {
        let u = rng.next_f64();
        u * u * u + 0.05
    })
    .collect();
    let sum: f64 = w.iter().sum();
    for x in w.iter_mut() {
        *x /= sum;
    }
    let mut used = 0usize;
    for i in 0..k {
        if left == 0 {
            break;
        }
        let want = if i + 1 == k {
            left - used.min(left)
        } else {
            ((budget - direct) as f64 * w[i]).round() as usize
        };
        let want = want.min(left - used.min(left));
        if want == 0 {
            continue;
        }
        let name = DIRS[(rng.next_u64() as usize) % DIRS.len()];
        let child = format!("{prefix}/{name}{}", if i == 0 { String::new() } else { format!("{i}") });
        let off = (rng.next_f64() * 260.0) as i64;
        gen_dir(&child, want, depth + 1, founded + off, out);
        used += want;
    }
    left = left.saturating_sub(used);
    if left > 0 {
        emit_files(prefix, left, founded, out);
    }
}

fn emit_files(prefix: &str, n: usize, founded: i64, out: &mut Vec<Raw>) {
    let p = LogicalPath::new(prefix).unwrap();
    let mut rng = SeededRng::for_path(&p, "synth-files");
    let mut seen: BTreeMap<String, u32> = BTreeMap::new();
    for _ in 0..n {
        let stem = STEMS[(rng.next_u64() as usize) % STEMS.len()];
        let ext = EXTS[(rng.next_u64() as usize) % EXTS.len()];
        let base = format!("{stem}.{ext}");
        let c = seen.entry(base.clone()).or_insert(0);
        let name = if *c == 0 {
            base.clone()
        } else {
            format!("{stem}_{c}.{ext}")
        };
        *c += 1;
        let u = rng.next_f64();
        let size = (180.0 + u * u * u * 46_000.0) as u64;
        let path = LogicalPath::new(&format!("{prefix}/{name}")).unwrap();
        let jitter = (rng.next_f64() * 90.0) as i64;
        out.push(Raw {
            path,
            size,
            founded: founded + jitter,
        });
    }
}

pub fn synth_repo(total: usize) -> RepoTree {
    // A monorepo shape: two dominant trees, several mid-size ones, a long tail.
    let plan: &[(&str, f64, i64)] = &[
        ("src", 0.215, 0),
        ("packages", 0.250, 340),
        ("vendor", 0.130, 120),
        ("tests", 0.115, 200),
        ("web", 0.058, 900),
        ("docs", 0.052, 1400),
        ("tools", 0.036, 700),
        ("migrations", 0.028, 480),
        ("examples", 0.028, 1600),
        ("scripts", 0.020, 260),
        ("benches", 0.014, 1750),
        ("config", 0.010, 40),
        ("infra", 0.010, 1900),
        ("proto", 0.016, 620),
        ("i18n", 0.010, 1500),
        ("assets", 0.008, 1200),
    ];
    let mut raw: Vec<Raw> = Vec::with_capacity(total);
    for (name, share, founded) in plan {
        let budget = (total as f64 * share).round() as usize;
        if *name == "packages" {
            // a dozen packages of very uneven size
            let mut rng = SeededRng::for_seed(0x9E37_79B9, "synth-packages");
            let names = [
                "ui", "cli", "sdk", "core-rt", "codegen", "lint", "fmt", "test-utils", "protocol",
                "storage", "auth", "telemetry",
            ];
            let mut w: Vec<f64> = names
                .iter()
                .map(|_| {
                    let u = rng.next_f64();
                    u * u + 0.06
                })
                .collect();
            let s: f64 = w.iter().sum();
            for x in w.iter_mut() {
                *x /= s;
            }
            for (i, pn) in names.iter().enumerate() {
                let b = (budget as f64 * w[i]).round() as usize;
                gen_dir(
                    &format!("packages/{pn}/src"),
                    b,
                    2,
                    founded + (i as i64) * 70,
                    &mut raw,
                );
            }
        } else {
            gen_dir(name, budget, 1, *founded, &mut raw);
        }
    }
    // a handful of root-level files: the civic square
    for n in [
        "README.md",
        "Cargo.toml",
        "package.json",
        "LICENSE",
        "Makefile",
        ".gitignore",
        "tsconfig.json",
    ] {
        raw.push(Raw {
            path: LogicalPath::new(n).unwrap(),
            size: 2400,
            founded: -20,
        });
    }

    // Growth order: directories arrive as units, in founding order.
    raw.sort_by(|a, b| {
        a.founded
            .cmp(&b.founded)
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut files: BTreeMap<LogicalPath, FileMeta> = BTreeMap::new();
    let base = 1_400_000_000i64; // 2014-ish
    for (i, r) in raw.iter().enumerate() {
        let ext = r.path.extension().unwrap_or("").to_owned();
        let t = base + r.founded * 36_000;
        files.insert(
            r.path.clone(),
            FileMeta {
                path: r.path.clone(),
                size_bytes: r.size,
                growth_index: i as u32,
                added_at: WallTime::from_unix_seconds(t),
                last_touched: WallTime::from_unix_seconds(t + 86_400),
                class: if r.path.as_str().starts_with("vendor/") {
                    FileClass::Industrial
                } else if r.path.depth() == 1 {
                    FileClass::CivicSquare
                } else {
                    FileClass::Ordinary
                },
                language: lang(&ext),
            },
        );
    }
    RepoTree {
        root: std::path::PathBuf::from("/synthetic"),
        files,
        worktrees: BTreeMap::new(),
        head: "synthetic".into(),
    }
}

/// Plausible cross-district import edges for the synthetic repo, deterministic.
pub fn synth_imports(tree: &RepoTree) -> Vec<(LogicalPath, LogicalPath)> {
    let paths: Vec<LogicalPath> = tree
        .files
        .values()
        .filter(|m| m.language.is_some())
        .map(|m| m.path.clone())
        .collect();
    if paths.len() < 4 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        let mut rng = SeededRng::for_path(p, "synth-import");
        let n = (rng.next_f64() * 4.0) as usize;
        for _ in 0..n {
            // mostly local, sometimes far: a realistic import profile
            let j = if rng.next_f64() < 0.72 {
                let span = 40usize;
                (i + 1 + (rng.next_u64() as usize) % span) % paths.len()
            } else {
                (rng.next_u64() as usize) % paths.len()
            };
            if j != i {
                out.push((p.clone(), paths[j].clone()));
            }
        }
    }
    out
}
