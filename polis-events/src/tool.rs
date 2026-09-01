//! Tool identity and call outcome (PRD §6.1 evidence weights, §10.1 glyphs,
//! §10.2 colour).
//!
//! # Two names that do not exist
//!
//! * **There is no `Task` tool.** The subagent-spawning tool is `Agent` — 169
//!   calls against 0 for `Task` across 37 917 `tool_use` blocks. `TaskCreate` /
//!   `TaskUpdate` / `TaskGet` / `TaskOutput` / `TaskStop` are an unrelated to-do
//!   list (`docs/verified/jsonl-schema.md` §6).
//! * **There is no `MultiEdit` tool.** The file-mutating built-ins are exactly
//!   `Edit`, `Write` and `NotebookEdit` (`docs/verified/hooks-schema.md` §6).
//!
//! A matcher on a tool that does not exist silently matches nothing, which is
//! why both are called out here rather than in a comment somewhere.
//!
//! # `PowerShell` is a first-class shell
//!
//! 2 137 `PowerShell` calls against 16 416 `Bash` in the local corpus, and on a
//! Windows box **without Git Bash, Claude Code does not register the `Bash` tool
//! at all** (`docs/verified/hooks-schema.md` §9.7). Every "is this a shell
//! command" branch must accept both, which is what [`ToolKind::is_shell`] is for.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A tool Claude Code can call.
///
/// Built-ins are enumerated; MCP tools and anything new arrive as
/// [`ToolKind::Mcp`] / [`ToolKind::Other`] so a Claude Code release that adds a
/// tool is a drift event, never a parse failure (PRD §17).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum ToolKind {
    /// `Read` — weak evidence; could be orientation (PRD §6.1, weight 1.0).
    Read,
    /// `Write` — commitment (PRD §6.1, weight 3.0). Mutating.
    Write,
    /// `Edit` — commitment (PRD §6.1, weight 3.0). Mutating.
    Edit,
    /// `NotebookEdit` — mutating. Present as a deferred tool; never invoked in
    /// the local corpus, but it is in the `PreToolUse` matcher.
    NotebookEdit,
    /// `Glob` — declares a scope as a pattern before any file returns.
    /// The strongest single signal (PRD §6.1, weight 5.0).
    Glob,
    /// `Grep` — same as [`ToolKind::Glob`] (PRD §6.1, weight 5.0).
    Grep,
    /// `Bash` — noisy; only its `cwd` is evidence (PRD §6.1, weight 0.5).
    Bash,
    /// `PowerShell` — the Windows shell tool. Identical weight to `Bash`.
    PowerShell,
    /// `Agent` — spawns a subagent. Rendered with the "delegate" glyph
    /// (PRD §10.1) and creates a [`crate::WorkerId`].
    Agent,
    /// `Workflow` — spawns the `workflow-subagent` fleet whose transcripts live
    /// under `subagents/workflows/wf_<runId>/`.
    Workflow,
    /// `Skill` — activates a packaged skill.
    Skill,
    /// `WebFetch`.
    WebFetch,
    /// `WebSearch`.
    WebSearch,
    /// `ToolSearch` — loads deferred tool schemas.
    ToolSearch,
    /// `StructuredOutput`. Its input schema is caller-defined with 160+ observed
    /// top-level keys; never model it.
    StructuredOutput,
    /// `TaskCreate` — the to-do list, not a subagent.
    TaskCreate,
    /// `TaskUpdate` — the to-do list, not a subagent.
    TaskUpdate,
    /// An MCP server tool, `mcp__<server>__<tool>`. Carries the full wire name.
    Mcp(String),
    /// Any tool name Polis does not know. Carries the wire name so the
    /// drill-down layer can still show it (PRD §12).
    Other(String),
}

impl ToolKind {
    /// Parses a wire tool name. Total: every string maps to something.
    pub fn parse(name: &str) -> Self {
        match name {
            "Read" => Self::Read,
            "Write" => Self::Write,
            "Edit" => Self::Edit,
            "NotebookEdit" => Self::NotebookEdit,
            "Glob" => Self::Glob,
            "Grep" => Self::Grep,
            "Bash" => Self::Bash,
            "PowerShell" => Self::PowerShell,
            "Agent" => Self::Agent,
            "Workflow" => Self::Workflow,
            "Skill" => Self::Skill,
            "WebFetch" => Self::WebFetch,
            "WebSearch" => Self::WebSearch,
            "ToolSearch" => Self::ToolSearch,
            "StructuredOutput" => Self::StructuredOutput,
            "TaskCreate" => Self::TaskCreate,
            "TaskUpdate" => Self::TaskUpdate,
            other if other.starts_with("mcp__") => Self::Mcp(other.to_owned()),
            other => Self::Other(other.to_owned()),
        }
    }

    /// The wire name, exactly as it appears in `tool_name` / `tool_use.name`.
    pub fn name(&self) -> &str {
        match self {
            Self::Read => "Read",
            Self::Write => "Write",
            Self::Edit => "Edit",
            Self::NotebookEdit => "NotebookEdit",
            Self::Glob => "Glob",
            Self::Grep => "Grep",
            Self::Bash => "Bash",
            Self::PowerShell => "PowerShell",
            Self::Agent => "Agent",
            Self::Workflow => "Workflow",
            Self::Skill => "Skill",
            Self::WebFetch => "WebFetch",
            Self::WebSearch => "WebSearch",
            Self::ToolSearch => "ToolSearch",
            Self::StructuredOutput => "StructuredOutput",
            Self::TaskCreate => "TaskCreate",
            Self::TaskUpdate => "TaskUpdate",
            Self::Mcp(s) | Self::Other(s) => s,
        }
    }

    /// The territory-inference weight from PRD §6.1's table.
    ///
    /// A tool with no path signal contributes `0.0` and is skipped rather than
    /// dropping a zero-weight kernel.
    pub fn evidence_weight(&self) -> f32 {
        match self {
            Self::Glob | Self::Grep => 5.0,
            Self::Write | Self::Edit | Self::NotebookEdit => 3.0,
            Self::Read => 1.0,
            Self::Bash | Self::PowerShell => 0.5,
            _ => 0.0,
        }
    }

    /// True for the tools that mutate a file — exactly the `PreToolUse` matcher
    /// `^(Edit|Write|NotebookEdit)$` that PRD §11.3's contention claims need.
    pub fn is_mutating(&self) -> bool {
        matches!(self, Self::Write | Self::Edit | Self::NotebookEdit)
    }

    /// True for `Bash` **and** `PowerShell`. Never test for `Bash` alone.
    pub fn is_shell(&self) -> bool {
        matches!(self, Self::Bash | Self::PowerShell)
    }

    /// True for the tools that create a [`crate::WorkerId`].
    pub fn spawns_worker(&self) -> bool {
        matches!(self, Self::Agent | Self::Workflow)
    }

    /// The operation glyph this tool renders as (PRD §10.1).
    ///
    /// **Shape encodes what, colour encodes how it went. Never conflate them** —
    /// so this deliberately takes no [`Outcome`].
    pub fn glyph(&self) -> Glyph {
        match self {
            Self::Edit | Self::NotebookEdit => Glyph::BarredCircle,
            Self::Write => Glyph::FilledSquare,
            Self::Bash | Self::PowerShell => Glyph::FilledTriangle,
            Self::Agent | Self::Workflow => Glyph::Delegate,
            // Read, Glob, Grep, ToolSearch, MCP tools and anything unknown all
            // read as "read / scan", which is the honest default for a tool
            // whose effect Polis does not model.
            _ => Glyph::HollowCircle,
        }
    }
}

impl fmt::Display for ToolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl From<String> for ToolKind {
    fn from(s: String) -> Self {
        Self::parse(&s)
    }
}

impl From<&str> for ToolKind {
    fn from(s: &str) -> Self {
        Self::parse(s)
    }
}

impl From<ToolKind> for String {
    fn from(t: ToolKind) -> Self {
        match t {
            ToolKind::Mcp(s) | ToolKind::Other(s) => s,
            other => other.name().to_owned(),
        }
    }
}

/// The shape channel of the visual language (PRD §10.1).
///
/// A `verify / test` glyph exists in the PRD table but has no tool of its own:
/// it is a shell call whose command ran the test suite, classified in
/// `polis-world`, so it is produced there rather than by [`ToolKind::glyph`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Glyph {
    /// read / scan.
    HollowCircle,
    /// edit.
    BarredCircle,
    /// write.
    FilledSquare,
    /// run (shell).
    FilledTriangle,
    /// verify / test.
    ConcentricCircles,
    /// delegate — centre dot with three satellites.
    Delegate,
}

/// The colour channel of the visual language (PRD §10.2).
///
/// Exactly three states. Colour is **never the sole channel** for any state
/// (PRD §11.4), so this is always paired with a [`Glyph`] and a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Outcome {
    /// Neutral. The call is in flight, or its result has not been seen.
    ///
    /// This is the default because absence of a result is the normal state on
    /// every channel: `tool_result` may be dropped, and `toolUseResult` is
    /// missing on 65% of subagent tool results.
    #[default]
    Pending,
    /// Teal. The call succeeded.
    Done,
    /// Red. The call failed, was rejected, or was aborted.
    Failed,
}

impl Outcome {
    /// Maps the channels' several spellings of success onto an outcome.
    ///
    /// `success` arrives as the **strings** `"true"` / `"false"` on OTLP, not as
    /// a bool; `is_error` is absent on 50% of transcript `tool_result` blocks and
    /// **absent means not-an-error, with no third state**. Both are handled here
    /// so no caller has to remember.
    pub fn from_success(success: Option<bool>) -> Self {
        match success {
            Some(true) => Self::Done,
            Some(false) => Self::Failed,
            None => Self::Pending,
        }
    }

    /// `is_error` semantics: `None` means the call did not fail.
    pub fn from_is_error(is_error: Option<bool>) -> Self {
        if is_error.unwrap_or(false) {
            Self::Failed
        } else {
            Self::Done
        }
    }

    /// True once the call has resolved either way.
    pub fn is_settled(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_round_trips_through_its_wire_name() {
        for name in [
            "Read",
            "Write",
            "Edit",
            "NotebookEdit",
            "Glob",
            "Grep",
            "Bash",
            "PowerShell",
            "Agent",
            "Workflow",
            "Skill",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "StructuredOutput",
            "TaskCreate",
            "TaskUpdate",
        ] {
            let k = ToolKind::parse(name);
            assert_eq!(k.name(), name);
            assert!(
                !matches!(k, ToolKind::Other(_)),
                "{name} fell through to Other"
            );
            assert_eq!(serde_json::to_string(&k).unwrap(), format!("\"{name}\""));
            assert_eq!(
                serde_json::from_str::<ToolKind>(&format!("\"{name}\"")).unwrap(),
                k
            );
        }
    }

    #[test]
    fn tools_that_do_not_exist_are_not_special_cased() {
        // A matcher on either of these silently matches nothing, which is the
        // whole point of asserting it.
        assert_eq!(ToolKind::parse("Task"), ToolKind::Other("Task".into()));
        assert_eq!(
            ToolKind::parse("MultiEdit"),
            ToolKind::Other("MultiEdit".into())
        );
        assert!(!ToolKind::parse("Task").spawns_worker());
        assert!(!ToolKind::parse("MultiEdit").is_mutating());
    }

    #[test]
    fn mcp_tools_are_distinguished_from_unknowns() {
        let m = ToolKind::parse("mcp__claude_ai_Gmail__send_message");
        assert!(matches!(m, ToolKind::Mcp(_)));
        assert_eq!(m.name(), "mcp__claude_ai_Gmail__send_message");
        assert!(weighs(&m, 0.0));
        assert!(matches!(
            ToolKind::parse("SomethingNew"),
            ToolKind::Other(_)
        ));
    }

    /// The weights are exact literals from a `match`, so bit equality is the
    /// right test — an epsilon comparison would also pass for a table mistyped
    /// by a rounding error.
    fn weighs(tool: &ToolKind, expected: f32) -> bool {
        tool.evidence_weight().to_bits() == expected.to_bits()
    }

    #[test]
    fn evidence_weights_match_the_prd_6_1_table() {
        assert!(weighs(&ToolKind::Glob, 5.0));
        assert!(weighs(&ToolKind::Grep, 5.0));
        assert!(weighs(&ToolKind::Edit, 3.0));
        assert!(weighs(&ToolKind::Write, 3.0));
        assert!(weighs(&ToolKind::Read, 1.0));
        assert!(weighs(&ToolKind::Bash, 0.5));
        // §9.7: the Bash row must read "Bash or PowerShell".
        assert!(weighs(
            &ToolKind::PowerShell,
            ToolKind::Bash.evidence_weight()
        ));
    }

    #[test]
    fn the_pretooluse_matcher_and_is_mutating_agree() {
        // settings.json registers `^(Edit|Write|NotebookEdit)$`.
        let matched = ["Edit", "Write", "NotebookEdit"];
        for name in matched {
            assert!(ToolKind::parse(name).is_mutating(), "{name}");
        }
        for name in ["Read", "Bash", "PowerShell", "Agent", "Glob", "MultiEdit"] {
            assert!(!ToolKind::parse(name).is_mutating(), "{name}");
        }
    }

    #[test]
    fn both_shells_are_shells() {
        assert!(ToolKind::Bash.is_shell());
        assert!(ToolKind::PowerShell.is_shell());
        assert!(!ToolKind::Read.is_shell());
    }

    #[test]
    fn glyphs_encode_operation_not_outcome() {
        assert_eq!(ToolKind::Read.glyph(), Glyph::HollowCircle);
        assert_eq!(ToolKind::Edit.glyph(), Glyph::BarredCircle);
        assert_eq!(ToolKind::Write.glyph(), Glyph::FilledSquare);
        assert_eq!(ToolKind::PowerShell.glyph(), Glyph::FilledTriangle);
        assert_eq!(ToolKind::Agent.glyph(), Glyph::Delegate);
    }

    #[test]
    fn outcome_handles_both_channel_conventions() {
        assert_eq!(Outcome::from_success(Some(true)), Outcome::Done);
        assert_eq!(Outcome::from_success(Some(false)), Outcome::Failed);
        // No result seen yet is Pending, not a failure.
        assert_eq!(Outcome::from_success(None), Outcome::Pending);
        // `is_error` absent means not-an-error; there is no third state.
        assert_eq!(Outcome::from_is_error(None), Outcome::Done);
        assert_eq!(Outcome::from_is_error(Some(false)), Outcome::Done);
        assert_eq!(Outcome::from_is_error(Some(true)), Outcome::Failed);
        assert_eq!(Outcome::default(), Outcome::Pending);
        assert!(!Outcome::Pending.is_settled());
        assert!(Outcome::Done.is_settled() && Outcome::Failed.is_settled());
    }
}
