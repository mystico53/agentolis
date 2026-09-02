//! Reading a real repository: the git growth order (PRD §7.1) and a cheap
//! import scan for streets (PRD §9).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use polis_events::LogicalPath;

use crate::accrete::FileRec;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("git runs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn is_industrial(p: &str) -> bool {
    p.starts_with("target/")
        || p.contains("/target/")
        || p.starts_with("node_modules/")
        || p.contains("/node_modules/")
        || p.ends_with(".lock")
        || p.ends_with("Cargo.lock")
        || p.contains("/vendor/")
        || p.starts_with("vendor/")
        || p.contains("/generated/")
}

fn is_monument(p: &str) -> bool {
    let f = p.rsplit('/').next().unwrap_or(p);
    matches!(
        f,
        "main.rs" | "lib.rs" | "index.ts" | "index.js" | "index.tsx" | "__init__.py" | "main.py"
    )
}

pub struct RepoData {
    pub files: Vec<FileRec>,
    pub imports: Vec<(u32, u32)>,
}

/// Replay `git log --diff-filter=A --name-only --reverse` — the growth order.
pub fn read(root: &Path) -> RepoData {
    let log = git(
        root,
        &[
            "log",
            "--diff-filter=A",
            "--name-only",
            "--reverse",
            "--format=%H|%ct",
            "--no-renames",
        ],
    );
    let mut order: Vec<(String, i64)> = Vec::new();
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();
    let mut ct: i64 = 0;
    for line in log.lines() {
        if let Some((_h, t)) = line.split_once('|') {
            if let Ok(v) = t.trim().parse::<i64>() {
                ct = v;
                continue;
            }
        }
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if seen.insert(l.to_string(), ()).is_none() {
            order.push((l.to_string(), ct));
        }
    }
    let index: BTreeMap<&str, (u32, i64)> = order
        .iter()
        .enumerate()
        .map(|(i, (p, t))| (p.as_str(), (i as u32, *t)))
        .collect();

    let tracked = git(root, &["ls-files"]);
    let mut recs: Vec<FileRec> = Vec::new();
    let mut path_of: BTreeMap<String, u32> = BTreeMap::new();
    let mut contents: Vec<String> = Vec::new();
    for line in tracked.lines() {
        let p = line.trim();
        if p.is_empty() {
            continue;
        }
        let Ok(lp) = LogicalPath::new(p) else { continue };
        let full = root.join(p);
        let size = std::fs::metadata(&full).map(|m| m.len()).unwrap_or(256);
        let (gi, at) = index.get(p).copied().unwrap_or((u32::MAX, 0));
        let body = if size < 400_000 {
            std::fs::read_to_string(&full).unwrap_or_default()
        } else {
            String::new()
        };
        path_of.insert(p.to_string(), recs.len() as u32);
        contents.push(body);
        recs.push(FileRec {
            path: lp,
            size_bytes: size.max(1),
            growth_index: gi,
            added_at: at,
            last_touched: at,
            industrial: is_industrial(p),
            monument: is_monument(p),
        });
    }
    // Untracked files still get a building; they sort last, by path.
    let mut untracked_rank = order.len() as u32;
    for (i, r) in recs.iter_mut().enumerate() {
        let _ = i;
        if r.growth_index == u32::MAX {
            r.growth_index = untracked_rank;
            untracked_rank += 1;
        }
    }
    let imports = scan_imports(&recs, &contents, &path_of);
    RepoData {
        files: recs,
        imports,
    }
}

/// Deliberately crude, and that is fine: PRD §9 says a file that cannot be
/// parsed simply has no streets.
fn scan_imports(
    recs: &[FileRec],
    contents: &[String],
    path_of: &BTreeMap<String, u32>,
) -> Vec<(u32, u32)> {
    // crate-name -> its crate root directory
    let mut crate_root: BTreeMap<String, String> = BTreeMap::new();
    for r in recs {
        let p = r.path.as_str();
        if let Some(dir) = p.strip_suffix("/src/lib.rs") {
            crate_root.insert(dir.replace('-', "_"), format!("{dir}/src"));
        }
    }
    let mut out: Vec<(u32, u32)> = Vec::new();
    for (i, body) in contents.iter().enumerate() {
        let src = &recs[i];
        let own_dir = src.path.parent().map(|p| p.as_str().to_string()).unwrap_or_default();
        let own_crate_src = own_dir
            .split("/src")
            .next()
            .map(|c| format!("{c}/src"))
            .unwrap_or(own_dir.clone());
        for line in body.lines().take(400) {
            let l = line.trim();
            let rest = if let Some(r) = l.strip_prefix("use ") {
                r
            } else if let Some(r) = l.strip_prefix("pub use ") {
                r
            } else {
                continue;
            };
            let head: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
                .collect();
            let mut parts = head.split("::").filter(|s| !s.is_empty());
            let Some(first) = parts.next() else { continue };
            let segs: Vec<&str> = parts.collect();
            let base = if first == "crate" || first == "self" || first == "super" {
                own_crate_src.clone()
            } else if let Some(b) = crate_root.get(first) {
                b.clone()
            } else {
                continue;
            };
            // Try the module file, then the module directory's mod.rs, then the
            // crate root itself.
            let mut cands: Vec<String> = Vec::new();
            if let Some(s0) = segs.first() {
                cands.push(format!("{base}/{s0}.rs"));
                cands.push(format!("{base}/{s0}/mod.rs"));
                if let Some(s1) = segs.get(1) {
                    cands.push(format!("{base}/{s0}/{s1}.rs"));
                }
            }
            cands.push(format!("{base}/lib.rs"));
            for c in cands {
                if let Some(&t) = path_of.get(&c) {
                    if t != i as u32 {
                        out.push((i as u32, t));
                    }
                    break;
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}
