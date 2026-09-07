//! Turning a program name into something a pty can actually start.
//!
//! Two rules live here, both Windows-shaped, both already learned once by
//! `polis_app::setup` for `std::process::Command` and needed again for a pty
//! that does not go through `Command` at all. They are **moved** here rather
//! than copied, and `polis_app::setup::{which, command_for}` delegate, so the
//! rule and its tests stay one thing (`docs/roadmap/terminal-integration.md`
//! asked for exactly this extraction).
//!
//! 1. **`PATH` is not `PATH` + `.exe`.** `std::process::Command` appends `.exe`
//!    and only `.exe`, so a `claude` installed by npm — which is `claude.cmd` —
//!    is invisible to it on a machine where `claude` works fine from the same
//!    shell. [`which`] honours `PATHEXT`.
//!
//! 2. **A `.cmd` is a script, not an image.** `CreateProcessW` refuses it, and
//!    `alacritty_terminal`'s `ConPTY` spawn is a bare `CreateProcessW`. [`resolve`]
//!    routes one through `cmd.exe /d /s /c`, which is the one documented form
//!    that treats the rest of the line verbatim.
//!
//! There is a third rule that only bites here: `alacritty_terminal` joins the
//! program and its arguments into a single command line and **does not quote the
//! program**, so `C:\Program Files\…\claude.exe` would arrive as two tokens.
//! [`resolve`] therefore quotes everything itself and the caller passes
//! `escape_args: false`.

use std::path::{Path, PathBuf};

/// A program resolved to something that can be started, with its arguments.
///
/// On Windows both fields are already quoted for a single command line, because
/// that is what the `ConPTY` spawn wants. On every other platform they are the
/// plain argv the caller passed, because `Command::args` does not want quoting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The image to start.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// The file [`which`] found, before any `cmd.exe` wrapping.
    ///
    /// Kept so the caller can say *what* it started in a status line, which
    /// `"cmd.exe"` would not.
    pub image: PathBuf,
}

/// Resolves a program name for a pty spawn.
///
/// # Errors
///
/// When the program is not on `PATH`. The message names the program, because
/// "program not found" without the name is the least useful sentence in a
/// terminal.
pub fn resolve(program: &str, args: &[String]) -> Result<Resolved, String> {
    let image = which(program)
        .ok_or_else(|| format!("{program} is not on PATH — install it, or give a full path"))?;

    #[cfg(windows)]
    {
        let script = image
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
        if script {
            let comspec = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_owned());
            // `/s` plus one outer pair of quotes is the documented form in which
            // `cmd.exe` strips exactly the outer quotes and takes the rest
            // verbatim. Anything else and a path with a space inside a quoted
            // argument gets re-split.
            let mut line = String::from("\"");
            line.push_str(&quote_for_cmd(&image.display().to_string()));
            for arg in args {
                line.push(' ');
                line.push_str(&quote_for_cmd(arg));
            }
            line.push('"');
            return Ok(Resolved {
                program: comspec,
                args: vec!["/d".to_owned(), "/s".to_owned(), "/c".to_owned(), line],
                image,
            });
        }
        Ok(Resolved {
            program: quote_argv(&image.display().to_string()),
            args: args.iter().map(|a| quote_argv(a)).collect(),
            image,
        })
    }

    #[cfg(not(windows))]
    {
        Ok(Resolved {
            program: image.display().to_string(),
            args: args.to_vec(),
            image,
        })
    }
}

/// Resolves a program name against `PATH`, honouring `PATHEXT` on Windows.
///
/// A path with a separator in it is taken as given and only checked for
/// existence — an operator who typed a path meant that path.
#[must_use]
pub fn which(program: &str) -> Option<PathBuf> {
    let raw = Path::new(program);
    if raw.is_absolute() || raw.components().count() > 1 {
        return raw.is_file().then(|| raw.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let direct = dir.join(program);
        if direct.is_file() && (!cfg!(windows) || raw.extension().is_some()) {
            return Some(direct);
        }
        for ext in path_extensions() {
            let candidate = dir.join(format!("{program}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// The extensions [`which`] appends. Empty on anything but Windows.
fn path_extensions() -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
        .split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Quotes one token for a `cmd.exe /s /c` command line.
///
/// Compiled on every platform so it stays tested on the ones that never call it.
#[must_use]
pub fn quote_for_cmd(token: &str) -> String {
    if !token.is_empty()
        && !token.contains([' ', '\t', '"', '&', '|', '<', '>', '^', '(', ')', ','])
    {
        return token.to_owned();
    }
    format!("\"{}\"", token.replace('"', "\"\""))
}

/// Quotes one token for a command line parsed by the C runtime's rules.
///
/// This is the rule `CreateProcessW`'s callee applies, and it is **not**
/// `cmd.exe`'s: backslashes double only in front of a quote. Used for a direct
/// spawn; [`quote_for_cmd`] is used inside a `cmd.exe /c` line.
#[must_use]
pub fn quote_argv(token: &str) -> String {
    if !token.is_empty() && !token.contains([' ', '\t', '"']) {
        return token.to_owned();
    }
    let mut out = String::with_capacity(token.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in token.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // n backslashes then a quote needs 2n+1 backslashes.
                for _ in 0..=backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('"');
            }
            other => {
                backslashes = 0;
                out.push(other);
            }
        }
    }
    // Trailing backslashes double, so the closing quote is not escaped.
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_token_is_left_alone_by_both_quoters() {
        assert_eq!(quote_for_cmd("claude"), "claude");
        assert_eq!(quote_argv("claude"), "claude");
        assert_eq!(quote_argv("--session-id"), "--session-id");
    }

    #[test]
    fn a_path_with_a_space_is_quoted_for_a_direct_spawn() {
        assert_eq!(
            quote_argv(r"C:\Program Files\nodejs\claude.exe"),
            r#""C:\Program Files\nodejs\claude.exe""#
        );
    }

    /// The C runtime rule: a backslash is literal unless it precedes a quote.
    /// Getting this wrong turns a Windows path into a broken escape.
    #[test]
    fn backslashes_only_double_in_front_of_a_quote() {
        assert_eq!(quote_argv(r"a\b c"), r#""a\b c""#);
        assert_eq!(quote_argv(r#"say "hi""#), r#""say \"hi\"""#);
        // Only when the token needs quoting at all does the trailing run
        // double, so the closing quote is not swallowed by it.
        assert_eq!(quote_argv(r"a b\"), r#""a b\\""#);
        assert_eq!(
            quote_argv(r"trail\"),
            r"trail\",
            "nothing to quote, nothing to escape"
        );
    }

    /// `cmd.exe` doubles a quote rather than escaping it, and cares about
    /// characters the C runtime does not.
    #[test]
    fn the_cmd_quoter_is_not_the_argv_quoter() {
        assert_eq!(quote_for_cmd("a&b"), "\"a&b\"");
        assert_eq!(quote_argv("a&b"), "a&b");
        assert_eq!(quote_for_cmd(r#"say "hi""#), r#""say ""hi""""#);
    }

    /// An empty argument has to survive, or `claude ""` becomes `claude`.
    #[test]
    fn an_empty_argument_survives() {
        assert_eq!(quote_argv(""), "\"\"");
        assert_eq!(quote_for_cmd(""), "\"\"");
    }

    /// A program that is not there must say so, by name.
    #[test]
    fn an_unknown_program_names_itself_in_the_error() {
        let error = resolve("polis-no-such-program-anywhere", &[]).expect_err("must fail");
        assert!(error.contains("polis-no-such-program-anywhere"), "{error}");
        assert!(error.contains("PATH"), "{error}");
    }

    /// The one program guaranteed present on each platform, so the happy path
    /// is covered on both CI legs.
    #[test]
    fn a_real_program_resolves_to_a_file_that_exists() {
        let program = if cfg!(windows) { "cmd" } else { "sh" };
        let resolved = resolve(program, &["-x".to_owned()]).expect("resolve");
        assert!(resolved.image.is_file(), "{:?}", resolved.image);
        assert_eq!(resolved.args.len(), 1);
    }

    /// A `.cmd` cannot be an image, and the wrapper is the whole reason an npm
    /// install of Claude Code works at all.
    #[cfg(windows)]
    #[test]
    fn a_cmd_script_goes_through_comspec() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake agent.cmd");
        std::fs::write(&script, "@echo off\r\n").expect("write");
        let resolved =
            resolve(&script.display().to_string(), &["--flag".to_owned()]).expect("resolve");
        assert!(
            resolved.program.to_ascii_lowercase().contains("cmd"),
            "{resolved:?}"
        );
        assert_eq!(&resolved.args[..3], ["/d", "/s", "/c"]);
        let line = &resolved.args[3];
        assert!(line.starts_with('"') && line.ends_with('"'), "{line}");
        assert!(line.contains("fake agent.cmd"), "{line}");
        assert!(line.contains("--flag"), "{line}");
        assert_eq!(resolved.image, script, "the image is still the script");
    }
}
