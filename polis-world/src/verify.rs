//! Which shell commands count as "tests ran" (PRD §10.1, §11.2).
//!
//! PRD §10.1 lists a **verify / test** glyph, and no tool owns it:
//!
//! > A `verify / test` glyph exists in the PRD table but has no tool of its own:
//! > it is a shell command whose command ran the test suite, classified in
//! > `polis-world`. (`polis_events::Glyph`)
//!
//! This is also the input to PRD §11.2's split of `done`:
//!
//! > *Done, verified* (tests ran against the changed files after the change):
//! > full weight 20s, then decays to base layer.
//! > *Done, unverified*: **persists.** This is really "needs review".
//!
//! # What this can and cannot know
//!
//! It classifies a **command line**, which is all any channel carries. It does
//! not know which files a test exercised, so [`crate::World`] applies the result
//! to every file the thread has touched and not yet verified — the honest
//! approximation of "tests ran against the changed files after the change". A
//! per-file coverage map would be a different product.
//!
//! The list is deliberately conservative. A false positive marks unreviewed work
//! as verified, which is the one direction that costs the operator something:
//! PRD §11.2 says *done, unverified* is "the second most important thing on the
//! map", and quietly promoting it to *done* is how it disappears.

use polis_events::{Glyph, ToolKind};

/// Command fragments that mean "the test suite ran".
///
/// Matched case-insensitively as substrings of the command line, because a real
/// command is `cd foo && cargo test --workspace 2>&1 | tail -40` far more often
/// than it is `cargo test`. Every entry is at least two tokens, so a directory
/// called `test` does not match.
pub const TEST_COMMANDS: &[&str] = &[
    "cargo test",
    "cargo nextest",
    "cargo miri test",
    "npm test",
    "npm run test",
    "npm t ",
    "yarn test",
    "pnpm test",
    "pnpm run test",
    "bun test",
    "go test",
    "dotnet test",
    "mvn test",
    "mvn verify",
    "gradle test",
    "gradlew test",
    "make test",
    "make check",
    "ctest",
    "pytest",
    "py.test",
    "python -m pytest",
    "python -m unittest",
    "python3 -m pytest",
    "tox",
    "rspec",
    "phpunit",
    "jest",
    "vitest",
    "mocha",
    "playwright test",
    "cypress run",
    "swift test",
    "zig build test",
    "rake test",
    "busted",
    "deno test",
];

/// True when a shell command line ran the test suite.
///
/// Accepts commands from `Bash` **and** `PowerShell` — on a Windows box without
/// Git Bash, Claude Code does not register the `Bash` tool at all (ADR-0032), so
/// a check that only looked at `Bash` would report every Windows session as
/// unverified.
pub fn is_verification_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    // Pad so a leading `npm t ` style needle can still match at the end.
    let padded = format!(" {lower} ");
    TEST_COMMANDS.iter().any(|needle| padded.contains(needle))
}

/// The glyph a tool call renders as, promoting a test run to the verify glyph
/// (PRD §10.1).
///
/// > **Shape encodes what, colour encodes how it went. Never conflate them.**
///
/// So this takes the command, never the outcome: a failing test run is still a
/// verify glyph, drawn red.
pub fn glyph_for(tool: &ToolKind, command: Option<&str>) -> Glyph {
    match command {
        Some(c) if tool.is_shell() && is_verification_command(c) => Glyph::ConcentricCircles,
        _ => tool.glyph(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_command_lines_are_recognised() {
        for cmd in [
            "cargo test --workspace",
            "cargo test --workspace 2>&1 | tail -40",
            "cd /c/coding/agentolis && cargo nextest run",
            "npm test",
            "npx vitest run src/",
            "python -m pytest tests/ -q",
            "PYTHONIOENCODING=utf-8 pytest -x",
            "go test ./...",
            "gradlew test --info",
        ] {
            assert!(is_verification_command(cmd), "{cmd}");
        }
    }

    #[test]
    fn ordinary_commands_are_not_verification() {
        // A false positive marks unreviewed work as reviewed, which is the one
        // direction that costs the operator something.
        for cmd in [
            "cargo build",
            "cargo clippy --workspace --all-targets",
            "git status",
            "ls tests",
            "cd tests && ls",
            "rg 'test' src/",
            "cat src/tests.rs",
        ] {
            assert!(!is_verification_command(cmd), "{cmd}");
        }
    }

    #[test]
    fn both_shells_get_the_verify_glyph_and_nothing_else_does() {
        assert_eq!(
            glyph_for(&ToolKind::Bash, Some("cargo test")),
            Glyph::ConcentricCircles
        );
        assert_eq!(
            glyph_for(&ToolKind::PowerShell, Some("cargo test")),
            Glyph::ConcentricCircles
        );
        assert_eq!(
            glyph_for(&ToolKind::Bash, Some("cargo build")),
            Glyph::FilledTriangle
        );
        assert_eq!(glyph_for(&ToolKind::Bash, None), Glyph::FilledTriangle);
        // A tool that is not a shell keeps its own glyph whatever the string is.
        assert_eq!(
            glyph_for(&ToolKind::Read, Some("cargo test")),
            Glyph::HollowCircle
        );
        assert_eq!(glyph_for(&ToolKind::Edit, None), Glyph::BarredCircle);
    }

    #[test]
    fn classification_is_case_insensitive_because_powershell_is() {
        assert!(is_verification_command("Cargo Test --Workspace"));
        assert!(is_verification_command("CARGO TEST"));
    }
}
