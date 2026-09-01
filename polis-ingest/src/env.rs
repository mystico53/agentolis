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

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use polis_events::EventKind;
use serde_json::{json, Map, Value};

/// Per-handler timeout, in seconds (`docs/verified/hooks-schema.md` §6.3).
///
/// The default is **600**, so a wedged hook would occupy a slot for ten minutes.
/// Five seconds is 400× the measured p95 spawn-to-exit of 14.6 ms.
pub const HOOK_TIMEOUT_SECS: u64 = 5;

/// The `PreToolUse` matcher (`docs/verified/hooks-schema.md` §6.6, ADR-0004).
///
/// **Anchored**, because a matcher containing regex characters is evaluated as
/// an *unanchored* JavaScript `RegExp`, and unanchored `Edit` also matches
/// `NotebookEdit`. The three file-mutating built-ins are exactly these; there is
/// no `MultiEdit` tool (ADR-0031).
pub const PRE_TOOL_USE_MATCHER: &str = "^(Edit|Write|NotebookEdit)$";

/// The `Notification` matcher — the three types that map to PRD §11.2's
/// attention states. `Notification` cannot block and its exit code is ignored,
/// so it is the safest event on the list.
pub const NOTIFICATION_MATCHER: &str = "permission_prompt|idle_prompt|agent_needs_input";

/// The variables Polis sets on a child agent process.
///
/// Returned as pairs rather than applied directly, so `polis env` can print them
/// and an operator can paste them into their own shell for an agent Polis did
/// not launch — which is the common case, since operators start `claude`
/// themselves.
///
/// Order is fixed and meaningful: master switches, exporter selection,
/// endpoint, cadence, content gates, privacy. It is also the order
/// `docs/verified/otel-schema.md` §1.6 verified end to end on this machine.
///
/// `otlp_endpoint` is a URL with **no path** — `http://127.0.0.1:4317` — because
/// this is the gRPC exporter. The HTTP exporter would want `/v1/logs` and Polis
/// does not run one.
pub fn agent_env(otlp_endpoint: &str) -> Vec<(String, String)> {
    let pairs: [(&str, &str); 12] = [
        // --- master switches (otel-schema.md §1.1) --------------------------
        ("CLAUDE_CODE_ENABLE_TELEMETRY", "1"),
        // REQUIRED for subagent attribution. Without it `OTEL_TRACES_EXPORTER`
        // is inert and no `claude_code.tool` span is ever produced, which is the
        // only place `agent_id` appears (ADR-0006).
        ("CLAUDE_CODE_ENHANCED_TELEMETRY_BETA", "1"),
        ("OTEL_LOGS_EXPORTER", "otlp"),
        ("OTEL_METRICS_EXPORTER", "otlp"),
        ("OTEL_TRACES_EXPORTER", "otlp"),
        // --- endpoint (§1.2) ------------------------------------------------
        // There is no default protocol; without this the otlp exporter does
        // nothing at all and Polis sees an empty, healthy-looking city.
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
        ("OTEL_EXPORTER_OTLP_ENDPOINT", otlp_endpoint),
        // --- cadence (§1.3) -------------------------------------------------
        // 5× and 6× faster than the 5 000 / 60 000 defaults, deliberately: a
        // city that lags a minute behind the agent is not glanceable.
        ("OTEL_LOGS_EXPORT_INTERVAL", "1000"),
        ("OTEL_TRACES_EXPORT_INTERVAL", "1000"),
        ("OTEL_METRIC_EXPORT_INTERVAL", "10000"),
        // --- content gates (§1.4) -------------------------------------------
        // The one Polis needs: it puts `file_path` on tool spans and
        // `tool_input` on tool events. OTEL_LOG_USER_PROMPTS,
        // OTEL_LOG_ASSISTANT_RESPONSES, OTEL_LOG_TOOL_CONTENT and
        // OTEL_LOG_RAW_API_BODIES are deliberately absent (ADR-0005).
        ("OTEL_LOG_TOOL_DETAILS", "1"),
        // --- privacy (§1.5, ADR-0005) ---------------------------------------
        ("OTEL_METRICS_INCLUDE_ACCOUNT_UUID", "false"),
    ];
    pairs
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// The names Polis deliberately leaves unset, with the reason.
///
/// Printed by `polis env` so the absence is documented rather than looking like
/// an oversight — every one of these leaks conversation content (ADR-0005).
pub const SUPPRESSED_ENV: &[(&str, &str)] = &[
    ("OTEL_LOG_USER_PROMPTS", "would export prompt text verbatim"),
    (
        "OTEL_LOG_ASSISTANT_RESPONSES",
        "would export response text verbatim",
    ),
    (
        "OTEL_LOG_TOOL_CONTENT",
        "would export tool input/output bodies as span events",
    ),
    (
        "OTEL_LOG_RAW_API_BODIES",
        "would export the entire conversation history",
    ),
];

/// Renders [`agent_env`] as a POSIX `export` block.
pub fn export_block_sh(otlp_endpoint: &str) -> String {
    let mut out = String::new();
    for (key, value) in agent_env(otlp_endpoint) {
        let _ = writeln!(out, "export {key}={value}");
    }
    out
}

/// Renders [`agent_env`] as a PowerShell `$env:` block.
///
/// Windows is the reference platform (`docs/verified/hooks-schema.md`), and an
/// operator on it needs this spelling, not the POSIX one.
pub fn export_block_powershell(otlp_endpoint: &str) -> String {
    let mut out = String::new();
    for (key, value) in agent_env(otlp_endpoint) {
        let _ = writeln!(out, "$env:{key} = \"{value}\"");
    }
    out
}

/// The `hooks` object `polis install-hooks` writes, pointing at `hook_binary`.
///
/// Nineteen registrations, derived from [`EventKind::ALL`] rather than written
/// out, so this block and the wire tag table cannot drift apart — and so
/// `WorktreeCreate`, which is not an [`EventKind`] variant, cannot be added here
/// by accident (ADR-0002).
///
/// Exec form (`args`), never a shell command: measured p50 per event is 5.1 ms
/// exec, 30.8 ms through Git Bash and 123.9 ms through PowerShell, so the
/// registration form costs more than everything the binary does (ADR-0016).
pub fn hooks_block(hook_binary: &Path) -> Value {
    let command = hook_binary.display().to_string();
    let mut hooks = Map::new();
    // Skipping `Unknown` (tag 0) leaves exactly the 19 registered events, in tag
    // order, which is also the order `docs/verified/hooks-schema.md` §6 lists.
    for kind in EventKind::ALL.iter().copied().skip(1) {
        let handler = json!({
            "type": "command",
            "command": command,
            "args": ["--event", kind.name()],
            "timeout": HOOK_TIMEOUT_SECS,
        });
        let group = match matcher_for(kind) {
            Some(matcher) => json!({ "matcher": matcher, "hooks": [handler] }),
            None => json!({ "hooks": [handler] }),
        };
        hooks.insert(kind.name().to_owned(), Value::Array(vec![group]));
    }
    Value::Object(hooks)
}

/// The matcher for an event, when it needs one.
///
/// Only two do. Everything else fires unmatched, which for these events is both
/// correct and cheap — none of them is a firehose.
fn matcher_for(kind: EventKind) -> Option<&'static str> {
    match kind {
        // Unmatched, this is the ~200 calls/sec firehose PRD §4.2 exists to
        // avoid; PRD §11.3's contention claim needs it anyway (ADR-0004).
        EventKind::PreToolUse => Some(PRE_TOOL_USE_MATCHER),
        EventKind::Notification => Some(NOTIFICATION_MATCHER),
        _ => None,
    }
}

/// Plans the `.claude/settings.json` edit without touching the filesystem.
///
/// This is what `polis install-hooks --dry-run` prints. Every conflict is
/// reported by name and nothing is replaced.
pub fn plan_hooks(settings_path: &Path, hook_binary: &Path) -> HookInstall {
    let existing = read_settings(settings_path);
    let mut install = HookInstall {
        settings_path: settings_path.to_path_buf(),
        ..HookInstall::default()
    };
    if let Some(Err(error)) = &existing {
        install
            .notes
            .push(format!("{} is not valid JSON ({error}); install-hooks will refuse to touch it rather than destroy it", settings_path.display()));
        install.blocked = true;
        return install;
    }
    let mut settings = match existing {
        Some(Ok(Value::Object(map))) => map,
        Some(Ok(_)) => {
            install.notes.push(format!(
                "{} does not contain a JSON object at the top level; refusing to overwrite it",
                settings_path.display()
            ));
            install.blocked = true;
            return install;
        }
        Some(Err(_)) => unreachable!("handled above"),
        None => {
            install.creates_file = true;
            Map::new()
        }
    };

    let ours = hooks_block(hook_binary);
    let Value::Object(ours) = ours else {
        unreachable!("hooks_block always returns an object")
    };
    let mut merged = match settings.remove("hooks") {
        Some(Value::Object(map)) => map,
        Some(other) => {
            install.notes.push(format!(
                "the existing `hooks` key is {} rather than an object; it will be replaced",
                type_name(&other)
            ));
            Map::new()
        }
        None => Map::new(),
    };

    // Key by key, never wholesale: another tool's registrations under events
    // Polis does not use must survive (PRD §4.2).
    for (event, group) in ours {
        match merged.get(&event) {
            None => install.added.push(event.clone()),
            Some(existing) if existing == &group => install.unchanged.push(event.clone()),
            Some(_) => install.conflicts.push(event.clone()),
        }
        merged.insert(event, group);
    }
    settings.insert("hooks".to_owned(), Value::Object(merged));

    if settings
        .get("disableAllHooks")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        install.notes.push(
            "`disableAllHooks` is true in this file: every hook stays registered and none will run"
                .to_owned(),
        );
    }
    install.notes.push(
        "in an INTERACTIVE session Claude Code runs no settings-file hook — including from \
         ~/.claude/settings.json — until the workspace trust dialog is accepted for this folder, \
         so the first launch after install can look like a silent failure (hooks-schema.md §5.6)"
            .to_owned(),
    );
    install.notes.push(
        "WorktreeCreate is deliberately NOT registered: a hook there replaces git's worktree \
         creation and a non-zero exit fails it (ADR-0002)"
            .to_owned(),
    );

    install.settings_json = to_pretty(&Value::Object(settings));
    install
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
///
/// Infallible by signature, so an I/O failure lands in
/// [`HookInstall::notes`] with [`HookInstall::written`] left `false`. It is a
/// report, not a `Result`, because the caller has to print the plan either way.
/// An existing file is copied to `<name>.polis-backup` first.
pub fn install_hooks(settings_path: &Path, hook_binary: &Path) -> HookInstall {
    let mut install = plan_hooks(settings_path, hook_binary);
    if install.blocked {
        return install;
    }
    if let Some(parent) = settings_path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                install
                    .notes
                    .push(format!("cannot create {}: {error}", parent.display()));
                return install;
            }
        }
    }
    if settings_path.exists() {
        let backup = backup_path(settings_path);
        match std::fs::copy(settings_path, &backup) {
            Ok(_) => install.backup = Some(backup),
            Err(error) => {
                install.notes.push(format!(
                    "cannot back up {} ({error}); refusing to overwrite it",
                    settings_path.display()
                ));
                return install;
            }
        }
    }
    match std::fs::write(settings_path, format!("{}\n", install.settings_json)) {
        Ok(()) => install.written = true,
        Err(error) => install
            .notes
            .push(format!("cannot write {}: {error}", settings_path.display())),
    }
    install
}

/// Where [`install_hooks`] copies a file before replacing it.
pub fn backup_path(settings_path: &Path) -> PathBuf {
    let mut name = settings_path.as_os_str().to_os_string();
    name.push(".polis-backup");
    PathBuf::from(name)
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
    /// Event names already registered exactly as Polis would write them. A
    /// re-run of `install-hooks` puts every event here and changes nothing.
    pub unchanged: Vec<String>,
    /// Warnings for the operator — notably that in an **interactive** session
    /// Claude Code runs no settings-file hook, including from
    /// `~/.claude/settings.json`, until the workspace trust dialog is accepted.
    /// Without saying so, the first launch after install looks like a silent
    /// failure.
    pub notes: Vec<String>,
    /// The exact file content `install_hooks` wrote, or `--dry-run` would write.
    pub settings_json: String,
    /// True when the settings file did not exist and would be created.
    pub creates_file: bool,
    /// Where the previous file was copied, when one existed.
    pub backup: Option<PathBuf>,
    /// Whether the file was actually written. Always `false` from
    /// [`plan_hooks`].
    pub written: bool,
    /// Set when Polis refused to touch the file at all — malformed JSON, or a
    /// top-level value that is not an object. Nothing was written and nothing
    /// should be.
    pub blocked: bool,
}

impl HookInstall {
    /// Whether anything Polis did not write would be replaced.
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }
}

/// `None` when the file does not exist; `Some(Err)` when it exists and is not
/// valid JSON — which must never be treated as "start from scratch", because
/// that silently deletes an operator's whole settings file.
fn read_settings(path: &Path) -> Option<Result<Value, serde_json::Error>> {
    let text = std::fs::read_to_string(path).ok()?;
    if text.trim().is_empty() {
        return Some(Ok(Value::Object(Map::new())));
    }
    Some(serde_json::from_str(&text))
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn to_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_block_carries_both_variables_subagent_attribution_needs() {
        let env = agent_env("http://127.0.0.1:4317");
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        // The single most consequential finding in otel-schema.md: without both
        // of these there is no `claude_code.tool` span and therefore no
        // `agent_id` anywhere in the telemetry (ADR-0006).
        assert_eq!(get("CLAUDE_CODE_ENHANCED_TELEMETRY_BETA"), Some("1"));
        assert_eq!(get("OTEL_TRACES_EXPORTER"), Some("otlp"));
        // Without a protocol the otlp exporter does nothing at all.
        assert_eq!(get("OTEL_EXPORTER_OTLP_PROTOCOL"), Some("grpc"));
        assert_eq!(
            get("OTEL_EXPORTER_OTLP_ENDPOINT"),
            Some("http://127.0.0.1:4317")
        );
        // The deliberate speed-ups, not boilerplate: 5× and 6× the defaults.
        assert_eq!(get("OTEL_LOGS_EXPORT_INTERVAL"), Some("1000"));
        assert_eq!(get("OTEL_METRIC_EXPORT_INTERVAL"), Some("10000"));
        assert_eq!(get("OTEL_METRICS_INCLUDE_ACCOUNT_UUID"), Some("false"));
        assert_eq!(get("OTEL_LOG_TOOL_DETAILS"), Some("1"));
    }

    #[test]
    fn nothing_that_exports_conversation_content_is_ever_set() {
        let env = agent_env("http://127.0.0.1:4317");
        for (name, _) in SUPPRESSED_ENV {
            assert!(
                !env.iter().any(|(key, _)| key == name),
                "{name} must never be set by Polis (ADR-0005)"
            );
        }
    }

    #[test]
    fn the_hooks_block_is_nineteen_events_in_exec_form_and_never_worktree_create() {
        let block = hooks_block(Path::new(r"C:\Users\op\.polis\bin\polis-hook.exe"));
        let map = block.as_object().expect("an object");
        assert_eq!(map.len(), 19, "hooks-schema.md §6: 19 registrations");
        assert!(
            !map.contains_key("WorktreeCreate"),
            "a WorktreeCreate hook replaces git's worktree creation and fails it (ADR-0002)"
        );
        assert!(!map.contains_key("FileChanged"), "ADR-0003");
        for (event, groups) in map {
            let groups = groups.as_array().expect("an array of matcher groups");
            assert_eq!(groups.len(), 1, "{event}");
            let handlers = groups[0]["hooks"].as_array().expect("handlers");
            assert_eq!(handlers.len(), 1, "{event}");
            let handler = &handlers[0];
            assert_eq!(handler["type"], "command");
            // Exec form: `args` present means no shell is involved at all
            // (ADR-0016). A `command` carrying flags would be shell form.
            let args = handler["args"].as_array().expect("exec form needs args");
            assert_eq!(args[0], "--event");
            assert_eq!(args[1].as_str(), Some(event.as_str()));
            assert_eq!(handler["timeout"], HOOK_TIMEOUT_SECS);
            assert!(
                !handler["command"].as_str().unwrap().contains("--"),
                "{event}: the command must be the bare executable"
            );
        }
    }

    #[test]
    fn only_pre_tool_use_and_notification_are_narrowed_and_pre_tool_use_is_anchored() {
        let block = hooks_block(Path::new("polis-hook"));
        let map = block.as_object().unwrap();
        let matcher = |event: &str| {
            map[event].as_array().unwrap()[0]
                .get("matcher")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        };
        // Anchored: unanchored `Edit` on the regex path also matches
        // `NotebookEdit` (hooks-schema.md §5.2).
        assert_eq!(matcher("PreToolUse").as_deref(), Some(PRE_TOOL_USE_MATCHER));
        assert!(PRE_TOOL_USE_MATCHER.starts_with('^') && PRE_TOOL_USE_MATCHER.ends_with('$'));
        assert!(
            !PRE_TOOL_USE_MATCHER.contains("MultiEdit"),
            "there is no MultiEdit tool (ADR-0031)"
        );
        assert_eq!(
            matcher("Notification").as_deref(),
            Some(NOTIFICATION_MATCHER)
        );
        for event in ["SessionStart", "Stop", "CwdChanged", "SubagentStop"] {
            assert_eq!(matcher(event), None, "{event} needs no matcher");
        }
    }

    #[test]
    fn a_clean_install_adds_nineteen_and_a_rerun_changes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = dir.path().join(".claude").join("settings.json");
        let hook = Path::new("polis-hook.exe");

        let plan = plan_hooks(&settings, hook);
        assert!(plan.creates_file);
        assert_eq!(plan.added.len(), 19);
        assert!(plan.conflicts.is_empty());
        assert!(!plan.written, "a plan never writes");
        assert!(!settings.exists(), "a plan never writes");

        let done = install_hooks(&settings, hook);
        assert!(done.written);
        assert!(
            done.backup.is_none(),
            "nothing to back up on a clean install"
        );

        let again = install_hooks(&settings, hook);
        assert_eq!(again.unchanged.len(), 19, "a re-run is idempotent");
        assert!(again.added.is_empty());
        assert!(again.conflicts.is_empty());
        assert!(
            again.backup.is_some(),
            "an existing file is always backed up"
        );
    }

    #[test]
    fn a_foreign_registration_is_reported_and_the_rest_of_the_file_survives() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = dir.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{
              "permissions": {"allow": ["Bash(git status)"]},
              "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "someone-elses-tool"}]}],
                "PostToolUse": [{"hooks": [{"type": "command", "command": "keep-me"}]}]
              }
            }"#,
        )
        .unwrap();

        let plan = plan_hooks(&settings, Path::new("polis-hook"));
        assert_eq!(
            plan.conflicts,
            vec!["Stop".to_owned()],
            "only the array Polis would replace is a conflict"
        );
        let written: Value = serde_json::from_str(&plan.settings_json).unwrap();
        // Key-by-key: an event Polis does not register keeps its handlers, and
        // the rest of the settings file is untouched.
        assert_eq!(
            written["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "keep-me"
        );
        assert_eq!(written["permissions"]["allow"][0], "Bash(git status)");
        assert_eq!(
            written["hooks"]["Stop"][0]["hooks"][0]["args"][1], "Stop",
            "the conflicting array is replaced in the plan, but only after being reported"
        );
    }

    #[test]
    fn a_malformed_settings_file_is_never_overwritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = dir.path().join("settings.json");
        std::fs::write(&settings, "{ this is not json").unwrap();
        let done = install_hooks(&settings, Path::new("polis-hook"));
        assert!(done.blocked);
        assert!(!done.written);
        assert_eq!(
            std::fs::read_to_string(&settings).unwrap(),
            "{ this is not json",
            "an operator's unreadable settings file is left exactly as it was"
        );
    }

    #[test]
    fn the_export_blocks_are_paste_able_in_both_shells() {
        let sh = export_block_sh("http://127.0.0.1:4317");
        assert!(sh.contains("export CLAUDE_CODE_ENABLE_TELEMETRY=1\n"));
        let ps = export_block_powershell("http://127.0.0.1:4317");
        assert!(ps.contains("$env:CLAUDE_CODE_ENABLE_TELEMETRY = \"1\"\n"));
        assert_eq!(sh.lines().count(), ps.lines().count());
        assert_eq!(sh.lines().count(), agent_env("x").len());
    }
}
