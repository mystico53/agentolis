//! Path evidence inside a shell command line (PRD §6.1).
//!
//! # Why this exists: measured, not imagined
//!
//! PRD §6.1's evidence table gives a shell call exactly one row — *"`Bash` cwd |
//! 0.5 | Noisy"* — so a session that works through the shell contributes one
//! observation per call, at its working directory. That was fine until two
//! things were measured together.
//!
//! First, the working directory is usually the **checkout root**, and the root
//! is the absorbing element of PRD §6.2's ancestor: `common_ancestor(root, x)`
//! is the root, so a single root-scoped observation pins `depth(A)` at 0 for as
//! long as it lives. [`crate::territory::Territory::observe`] therefore counts a
//! root-scoped observation and then drops it, for the same reason
//! [`crate::place`]'s rung 2 excludes a root `cwd`: *the whole city is not a
//! location.*
//!
//! Second — and this is the number that made the module — **three real agents
//! were started in one checkout with no configuration at all, and between them
//! made nine tool calls, every single one of which was `Bash`.** Not one `Read`,
//! not one `Glob`, not one `Edit`. Their commands were `ls -la src/auth`,
//! `find docs -type f | sort`, `wc -l docs/README.md docs/guide.md`,
//! `for f in src/auth/login.rs src/auth/session.rs src/auth/token.rs; do …`.
//!
//! Put together: three agents working in three obviously different districts,
//! and **no territory converged for any of them**. Every one rendered as an
//! unplaced marker in the status rail. PRD §6.2's rule was working exactly as
//! written — no agreement, therefore no cloud — and the map was still empty,
//! because the evidence had been thrown away before the rule ever saw it.
//!
//! The evidence is right there in the command line. `wc -l docs/README.md` is a
//! statement about scope in the same way `Read docs/README.md` is; it is simply
//! spelled in `sh` instead of JSON.
//!
//! # What keeps this from inventing scope
//!
//! A command line is not a path list, and a naive scan of it would turn
//! `find . -type d -name render` into evidence for `render`, `-type` and `.`.
//! Three gates stop that, and the last one is the load-bearing one:
//!
//! 1. **The first token of each command segment is skipped.** It is the
//!    executable — `find`, `cat`, `ls`, `wc`, `sort` — not an argument. Segments
//!    break on `;`, `|`, `&`, `(`, `)` and newlines.
//! 2. **Obvious non-paths are dropped**: flags, variables, globs, redirections,
//!    `.` and `..`, and anything with a shell metacharacter left in it.
//! 3. **A token must resolve to a path the city already knows** — an existing
//!    building or an existing district — or it contributes nothing. This is what
//!    separates `docs/README.md` from `render`: a bare word that happens to name
//!    a directory *is* scope evidence, and a bare word that names nothing is a
//!    word. Nothing here can create a claim on a path that does not exist.
//!
//! The result is weighted at [`SHELL_ARGUMENT_WEIGHT`] — the same 0.5 PRD §6.1
//! gives the `cwd` row, because it is the same evidence, only sharper. It is
//! **not** promoted to `Read`'s 1.0: the agent did not necessarily read the
//! file, and over-weighting the noisiest channel is how a territory ends up
//! shaped like whatever the last shell command happened to mention.
//!
//! # What this deliberately does not change
//!
//! Where the operation is **drawn**. [`crate::place`]'s four-rung chain is
//! untouched: a `Bash` call still places by its `cwd` or on its thread, and the
//! measured placement census does not move. This module answers *"what is this
//! thread's scope"*, which is a different question from *"where does this mark
//! go"*, and PRD §6 is emphatic that the first one is a density field rather
//! than a point.

use std::collections::BTreeSet;

use polis_events::LogicalPath;

use crate::{PathScope, World};

/// Evidence weight of a path named in a shell command's arguments.
///
/// PRD §6.1's `Bash` row, unchanged: *"cwd | 0.5 | Noisy"*. A sharper reading of
/// the same call is not a stronger kind of evidence.
pub const SHELL_ARGUMENT_WEIGHT: f32 = 0.5;

/// Most paths taken from one command line.
///
/// A command can name a hundred files — `wc -l $(git ls-files)` expands to the
/// whole repository — and a territory built from one such call would be the
/// whole city, which is PRD §6's opening failure. Eight is above every real
/// command measured on the operator's corpus and far below a flood.
pub const MAX_SHELL_PATHS: usize = 8;

/// Shallowest claim a shell argument may contribute, and why there is one.
///
/// Measured, on a real run. A subagent asked to look at `src/notes` opened with
/// `ls -laR .../src` and then went straight to `find src/notes -type f`. Its two
/// pieces of evidence were `src` (depth 1) and `src/notes` (depth 2); PRD §6.2
/// takes their lowest common ancestor, which is `src`, and `depth(A) >= 2`
/// fails. **The orientation command suppressed the territory the work
/// declared** — and it could never have produced one itself, because a claim at
/// depth 1 cannot pass the same gate.
///
/// Evidence that cannot create a territory but can prevent one is asymmetric
/// noise, and PRD §6.1 labels this exact channel *"Noisy"*. So a shell argument
/// whose *claim* — a file's parent, a directory itself — is shallower than
/// [`crate::territory::MIN_CLAIM_DEPTH`] contributes nothing.
///
/// This changes **only** what the shell channel adds. A `Read` or an `Edit` of
/// `src/lib.rs` still claims `src` and still pulls the ancestor up, because that
/// is real work at that depth and PRD §6.2's answer to it — no cloud — is the
/// PRD's own judgement about how much a whole `src/` tells an operator.
///
/// The consequence to be honest about: in a repository whose directories are all
/// one level deep, the shell channel contributes nothing at all. That is the
/// same repository in which PRD §6.2 already refuses to emit any territory.
pub const MIN_EVIDENCE_DEPTH: usize = crate::territory::MIN_CLAIM_DEPTH;

/// Longest command line scanned.
///
/// `polis-ingest` elides string leaves over 2 048 characters, so this is a
/// backstop for the hook channel, which does not.
const MAX_COMMAND_BYTES: usize = 8 * 1024;

impl World {
    /// The paths a shell command names that this city already knows about.
    ///
    /// Returns each path once, in the order the command mentions it, with the
    /// scope it claims: a file claims its parent directory, a directory claims
    /// itself (PRD §6.2's ancestor is taken over the claim, so a single file
    /// cannot become a territory).
    ///
    /// Empty is the common and correct answer for a command that names nothing
    /// in the repository — `git status`, `cargo build`, `npm ci`.
    pub(crate) fn shell_evidence(
        &mut self,
        cwd: Option<&str>,
        command: &str,
    ) -> Vec<(LogicalPath, PathScope)> {
        let mut out: Vec<(LogicalPath, PathScope)> = Vec::new();
        let mut seen: BTreeSet<LogicalPath> = BTreeSet::new();
        for token in argument_tokens(command) {
            if out.len() >= MAX_SHELL_PATHS {
                break;
            }
            // Resolved without `World::resolve_path`, on purpose: that counts
            // every miss in `Health::unmapped_paths`, and most tokens in a
            // command line are *supposed* to miss. Counting them would turn a
            // healthy channel into a permanently alarming one.
            let Some(token) = literal_prefix(token) else {
                continue;
            };
            let Some((_, path)) = self
                .mapper
                .resolve(cwd.map(std::path::Path::new), token)
                .or_else(|| {
                    LogicalPath::new(token)
                        .ok()
                        .map(|p| (polis_events::WorktreeId::PRIMARY, p))
                })
            else {
                continue;
            };
            if path.is_root() {
                continue;
            }
            let Some(scope) = self.known_scope(&path) else {
                continue;
            };
            if scope.claim_of(&path).depth() < MIN_EVIDENCE_DEPTH {
                continue;
            }
            if seen.insert(path.clone()) {
                out.push((path, scope));
            }
        }
        out
    }

    /// Whether the city knows this path, and as what.
    ///
    /// The anti-fabrication gate. A building answers [`PathScope::File`], a
    /// district answers [`PathScope::Directory`], and anything else answers
    /// `None` and contributes nothing.
    fn known_scope(&self, path: &LogicalPath) -> Option<PathScope> {
        if self.layout.buildings.contains_key(path) || self.repo.files.contains_key(path) {
            return Some(PathScope::File);
        }
        if self.layout.districts.contains_key(path) {
            return Some(PathScope::Directory);
        }
        None
    }
}

/// The argument tokens of a shell command line, executables excluded.
///
/// Gates 1 and 2 of this module's three; gate 3 needs the city and lives in
/// [`World::shell_evidence`].
fn argument_tokens(command: &str) -> Vec<&str> {
    let command = &command[..command.len().min(MAX_COMMAND_BYTES)];
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut quote: Option<char> = None;
    // The first token after a segment break is the executable, not an argument.
    let mut want_executable = true;

    let push = |start: &mut Option<usize>, end: usize, want_exec: &mut bool, out: &mut Vec<_>| {
        let Some(s) = start.take() else {
            return;
        };
        let token = &command[s..end];
        if token.is_empty() {
            return;
        }
        if *want_exec {
            // `VAR=value cmd args` — an assignment prefix is not the executable,
            // so the next token still is.
            if !token.contains('=') {
                *want_exec = false;
            }
            return;
        }
        if is_plausible_path(token) {
            out.push(token);
        }
    };

    for (i, ch) in command.char_indices() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                // A quote opens a token even when it is empty, so `"src/a b.rs"`
                // keeps its space; the quote characters themselves are trimmed
                // by `is_plausible_path`'s caller below.
                if start.is_none() {
                    start = Some(i + ch.len_utf8());
                }
            }
            ';' | '|' | '&' | '(' | ')' | '\n' | '\r' | '{' | '}' => {
                push(&mut start, i, &mut want_executable, &mut out);
                want_executable = true;
            }
            c if c.is_whitespace() => {
                push(&mut start, i, &mut want_executable, &mut out);
            }
            _ => {
                if start.is_none() {
                    start = Some(i);
                }
            }
        }
    }
    push(&mut start, command.len(), &mut want_executable, &mut out);
    out
}

/// The part of a token before its first glob metacharacter, as a directory.
///
/// A shell glob is not a path, but it is not nothing either — PRD §6.1 puts a
/// **pattern** at the top of its evidence table:
///
/// > `Glob` / `Grep` with path scope | **5.0** | Declares a scope as a pattern,
/// > before any file returns. Strongest single signal.
///
/// `wc -l src/render/*` declares `src/render` in exactly that sense, and it is
/// what a real agent wrote when asked to look at a directory. So the literal
/// prefix is kept and the pattern part is discarded — Polis does not, and must
/// not, decide what a glob matched: that is a filesystem question, and asking it
/// per tool call would put a `read_dir` on the ingest path.
///
/// It is **not** promoted to the table's 5.0. The `Glob` tool's `path` is a
/// scope the model declared to a tool; this is a fragment of a shell word, and
/// the difference is exactly the difference between the two rows in that table.
/// It keeps [`SHELL_ARGUMENT_WEIGHT`].
///
/// `None` when nothing literal survives — `*.rs`, `"*/node_modules/*"` — because
/// a pattern with no directory in it declares no scope.
fn literal_prefix(token: &str) -> Option<&str> {
    let Some(meta) = token.find(['*', '?', '[', ']']) else {
        return Some(token);
    };
    let head = &token[..meta];
    let cut = head.rfind(['/', '\\'])?;
    let dir = &head[..cut];
    (!dir.is_empty()).then_some(dir)
}

/// Whether a token could be a path at all.
///
/// Gate 2. Deliberately conservative in both directions: it lets bare
/// directory names through, because `ls src/auth` is exactly the evidence this
/// module exists for, and it relies on gate 3 to reject the ones that name
/// nothing.
fn is_plausible_path(token: &str) -> bool {
    if token.is_empty() || token == "." || token == ".." {
        return false;
    }
    // A flag, a redirection, a file descriptor, a variable, a glob, a
    // substitution, a URL, or an option value. None of these are paths, and a
    // glob is a *pattern* over paths rather than one — resolving it would mean
    // deciding what it matched, which is a filesystem question this must not ask
    // on a render thread.
    let first = token.as_bytes()[0];
    if matches!(first, b'-' | b'+' | b'$' | b'~' | b'<' | b'>' | b'`' | b'%') {
        return false;
    }
    if token
        .bytes()
        .any(|b| matches!(b, b'$' | b'`' | b'!' | b'=' | b'^'))
    {
        return false;
    }
    if token.contains("://") {
        return false;
    }
    // `2>/dev/null` and friends: a digit followed by a redirection.
    if first.is_ascii_digit() && token.contains('>') {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;
    use polis_repo::{FileMeta, RepoTree};

    #[test]
    fn the_executable_is_not_an_argument() {
        assert_eq!(argument_tokens("ls -la src/auth"), vec!["src/auth"]);
        assert_eq!(
            argument_tokens("cat docs/README.md"),
            vec!["docs/README.md"]
        );
        // `build` survives gate 2 — it is a bare word that *could* be a
        // directory — and is stopped by gate 3, which needs a city.
        assert_eq!(argument_tokens("cargo build"), vec!["build"]);
    }

    #[test]
    fn every_segment_has_its_own_executable() {
        // Real commands, from the three real agents measured on this machine.
        assert_eq!(
            argument_tokens("cat docs/README.md; echo \"=====SPLIT=====\"; cat docs/guide.md"),
            vec!["docs/README.md", "docs/guide.md"],
        );
        assert_eq!(
            argument_tokens("ls -la src/render 2>&1 | head -50"),
            vec!["src/render"],
        );
        assert_eq!(
            argument_tokens("wc -l docs/README.md docs/guide.md"),
            vec!["docs/README.md", "docs/guide.md"],
        );
    }

    #[test]
    fn a_loop_over_files_yields_the_files() {
        // `f`, `in` and `cat` come through gate 2 and die at gate 3.
        assert_eq!(
            argument_tokens(
                "for f in src/auth/login.rs src/auth/session.rs src/auth/token.rs; do cat $f; done"
            ),
            vec![
                "f",
                "in",
                "src/auth/login.rs",
                "src/auth/session.rs",
                "src/auth/token.rs",
                "cat",
            ],
        );
    }

    #[test]
    fn flags_redirections_globs_and_the_current_directory_are_not_paths() {
        // `find . -type d -name render 2>/dev/null | head -20` — the command
        // that would fabricate a territory called `render` without gate 3.
        let tokens = argument_tokens("find . -type d -name render 2>/dev/null | head -20");
        assert!(!tokens.contains(&"."), "the whole city is not a location");
        assert!(!tokens.contains(&"-type"));
        assert!(!tokens.contains(&"2>/dev/null"));
        assert!(
            tokens.contains(&"render"),
            "gate 3 is the one that rejects it"
        );

        assert_eq!(literal_prefix("src/**/*.rs"), Some("src"));
        assert_eq!(literal_prefix("src/render/*"), Some("src/render"));
        assert_eq!(literal_prefix("*/node_modules/*"), None);
        assert_eq!(literal_prefix("*.rs"), None);
        assert!(!is_plausible_path("$HOME/x"));
        assert!(!is_plausible_path("https://example.com/a"));
        assert!(!is_plausible_path("--workspace"));
        assert!(!is_plausible_path(".."));
    }

    #[test]
    fn an_environment_prefix_does_not_hide_the_executable() {
        assert_eq!(
            argument_tokens("RUST_LOG=debug cargo test polis-world/src/lib.rs"),
            vec!["test", "polis-world/src/lib.rs"],
        );
    }

    /// A city with two districts and three buildings, for gate 3.
    fn city() -> World {
        let mut tree = RepoTree {
            root: std::path::PathBuf::from("C:/repo"),
            ..RepoTree::default()
        };
        for path in [
            "src/auth/token.rs",
            "src/auth/session.rs",
            "src/render/frame.rs",
        ] {
            let logical = LogicalPath::new(path).expect("test path");
            tree.files
                .insert(logical.clone(), FileMeta::untracked(logical, 2_000));
        }
        let layout = polis_layout::city::generate(&tree);
        World::new(tree, layout)
    }

    #[test]
    fn gate_three_keeps_only_paths_the_city_already_knows() {
        let mut world = city();
        let cwd = Some("C:/repo");

        let hits = world.shell_evidence(cwd, "find . -type d -name render 2>/dev/null");
        assert!(
            hits.is_empty(),
            "a bare word that names nothing must not become a territory: {hits:?}"
        );

        let hits = world.shell_evidence(cwd, "ls -la src/render");
        assert_eq!(
            hits,
            vec![(
                LogicalPath::new("src/render").unwrap(),
                PathScope::Directory
            )],
            "a bare word that names a real district is scope evidence"
        );

        let hits = world.shell_evidence(cwd, "wc -l src/auth/token.rs src/auth/session.rs");
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|(_, s)| *s == PathScope::File));
    }

    #[test]
    fn an_orientation_listing_cannot_suppress_the_territory_the_work_declares() {
        // The measured regression: `ls -laR .../src` then `find src/notes -type f`
        // gave a lowest common ancestor of `src`, depth 1, and no cloud.
        let mut tree = RepoTree {
            root: std::path::PathBuf::from("C:/repo"),
            ..RepoTree::default()
        };
        for path in ["src/notes/one.md", "src/notes/two.md", "src/auth/token.rs"] {
            let logical = LogicalPath::new(path).expect("test path");
            tree.files
                .insert(logical.clone(), FileMeta::untracked(logical, 100));
        }
        let layout = polis_layout::city::generate(&tree);
        let mut world = World::new(tree, layout);

        assert!(
            world
                .shell_evidence(Some("C:/repo"), "ls -laR src")
                .is_empty(),
            "a claim at depth 1 can never be emitted, so it may not veto one that can"
        );
        assert_eq!(
            world.shell_evidence(Some("C:/repo"), "find src/notes -type f"),
            vec![(LogicalPath::new("src/notes").unwrap(), PathScope::Directory)],
        );
    }

    #[test]
    fn one_command_cannot_flood_a_territory() {
        let mut tree = RepoTree {
            root: std::path::PathBuf::from("C:/repo"),
            ..RepoTree::default()
        };
        for i in 0..40 {
            let logical = LogicalPath::new(&format!("src/gen/f{i}.rs")).expect("test path");
            tree.files
                .insert(logical.clone(), FileMeta::untracked(logical, 100));
        }
        let layout = polis_layout::city::generate(&tree);
        let mut world = World::new(tree, layout);
        let mut command = String::from("wc -l");
        for i in 0..40 {
            let _ = write!(command, " src/gen/f{i}.rs");
        }
        assert_eq!(
            world.shell_evidence(Some("C:/repo"), &command).len(),
            MAX_SHELL_PATHS
        );
    }

    #[test]
    fn a_command_that_names_nothing_contributes_nothing() {
        let mut world = city();
        assert!(world
            .shell_evidence(Some("C:/repo"), "git status --short")
            .is_empty());
        assert!(world
            .shell_evidence(Some("C:/repo"), "cargo build --release")
            .is_empty());
        assert!(world.shell_evidence(Some("C:/repo"), "").is_empty());
    }
}
