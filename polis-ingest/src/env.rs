//! The environment block injected into agents, and hook registration
//! (PRD §4.1, §4.2).
//!
//! Four corrections to PRD §4.1's eight-variable list, each verified:
//!
//! 1. **Tracing must be enabled.** On the logs channel a subagent's tool call is
//!    byte-for-byte indistinguishable from the main agent's — `agent_id` and
//!    `query_source` occur **zero** times across every `tool_decision` and
//!    `tool_result`. `agent_id` appears only on the `claude_code.tool` **span**.
//!    Polis's thread model therefore rests on a beta signal (ADR-0006).
//! 2. **Temporality is pinned explicitly** rather than relied on as a default,
//!    because managed settings can flip it (ADR-0007).
//! 3. **Account UUIDs are suppressed**, `user.email` is dropped at ingest, and
//!    prompt/response logging stays off (ADR-0005).
//! 4. **There is no default protocol.** Without `OTEL_EXPORTER_OTLP_PROTOCOL`
//!    the otlp exporter does nothing at all.
//!
//! The PRD's `OTEL_LOGS_EXPORT_INTERVAL=1000` and
//! `OTEL_METRIC_EXPORT_INTERVAL=10000` are a deliberate 5× and 6× speed-up over
//! the 5 000 / 60 000 defaults, not boilerplate.

use std::path::{Path, PathBuf};

/// The variables Polis sets on a child agent process.
///
/// Returned as pairs rather than applied directly, so `polis doctor` can print
/// them and an operator can paste them into their own shell for an agent Polis
/// did not launch — which is the common case, since operators start `claude`
/// themselves.
pub fn agent_env(otlp_endpoint: &str) -> Vec<(String, String)> {
    let _ = otlp_endpoint;
    todo!("PRD §4.1 as corrected by ADR-0005, ADR-0006 and ADR-0007")
}

/// Writes the `.claude/settings.json` hooks block (PRD §4.2).
///
/// Three constraints that are not negotiable:
///
/// * **Exec form** (`"args": [...]`), never a shell command. Measured p50 per
///   event: exec 5.1 ms, Git Bash `-c` 30.8 ms (6.1×), PowerShell `-Command`
///   123.9 ms (24.5×). The registration form costs more than everything the
///   binary does (ADR-0016).
/// * **No `WorktreeCreate` entry.** It would break every worktree on the machine
///   (ADR-0002).
/// * **`PreToolUse` narrowed to `^(Edit|Write|NotebookEdit)$`.** Unmatched, it is
///   the ~200 calls/sec firehose PRD §4.2 exists to avoid. There is no
///   `MultiEdit` tool; anchoring matters because unanchored `Edit` also matches
///   `NotebookEdit`.
///
/// Merges key-by-key into an existing `hooks` object and reports, rather than
/// silently replaces, any handler array Polis did not write.
pub fn install_hooks(settings_path: &Path, hook_binary: &Path) -> HookInstall {
    let _ = (settings_path, hook_binary);
    todo!("PRD §4.2 — the 19 registrations in docs/verified/hooks-schema.md §6")
}

/// What `polis install-hooks` did, or would do under `--dry-run`.
#[derive(Debug, Clone, Default)]
pub struct HookInstall {
    /// The settings file that was, or would be, written.
    pub settings_path: PathBuf,
    /// Event names newly registered. Nineteen on a clean install.
    pub added: Vec<String>,
    /// Event names whose existing handler array Polis would replace. Never
    /// replaced without explicit confirmation.
    pub conflicts: Vec<String>,
    /// Warnings for the operator — notably that in an **interactive** session
    /// Claude Code runs no settings-file hook, including from
    /// `~/.claude/settings.json`, until the workspace trust dialog is accepted.
    /// Without saying so, the first launch after install looks like a silent
    /// failure.
    pub notes: Vec<String>,
}
