# Verified: Claude Code hooks schema (Channel B)

**Status:** verified 2026-09-01 against the live hooks reference.
**Primary source:** `https://code.claude.com/docs/en/hooks` (raw markdown fetched from `https://code.claude.com/docs/en/hooks.md`, HTTP 200, 316,963 bytes).
**Secondary source:** `https://code.claude.com/docs/en/hooks-guide`.
**Local Claude Code version on the dev box:** `2.1.248`.
**Target platform:** `x86_64-pc-windows-msvc`, Windows 11 Pro 26200.

Everything in this file is quoted or paraphrased from the fetched reference, or measured on this machine.
Where the PRD conflicts with the reference, this file wins. See [§9 PRD corrections](#9-prd-corrections).

---

## 1. Complete list of hook event names

There are **32** hook events. This is the full set as of the fetched reference:

| # | Event | Fires when |
|---|---|---|
| 1 | `SessionStart` | A session begins or resumes |
| 2 | `Setup` | `--init-only`, or `--init` / `--maintenance` under `-p` |
| 3 | `UserPromptSubmit` | A prompt is submitted, before Claude processes it |
| 4 | `UserPromptExpansion` | A user-typed command expands into a prompt |
| 5 | `PreToolUse` | Before a tool call executes. **Can block** |
| 6 | `PermissionRequest` | A tool call needs a permission decision |
| 7 | `PermissionDenied` | Auto mode denies a tool call |
| 8 | `PostToolUse` | After a tool call succeeds |
| 9 | `PostToolUseFailure` | After a tool call fails |
| 10 | `PostToolBatch` | After a full batch of parallel tool calls resolves |
| 11 | `Notification` | Claude Code sends a notification |
| 12 | `MessageDisplay` | While assistant message text is displayed |
| 13 | `SubagentStart` | A subagent is spawned |
| 14 | `SubagentStop` | A subagent finishes |
| 15 | `TaskCreated` | A task is created via `TaskCreate` |
| 16 | `TaskCompleted` | A task is marked completed |
| 17 | `Stop` | Claude finishes responding |
| 18 | `StopFailure` | The turn ends due to an API error |
| 19 | `TeammateIdle` | An agent-team teammate is about to go idle |
| 20 | `InstructionsLoaded` | A CLAUDE.md or `.claude/rules/*.md` file loads into context |
| 21 | `ConfigChange` | A configuration file changes mid-session |
| 22 | `CwdChanged` | The working directory changes |
| 23 | `DirectoryAdded` | A working directory is added mid-session |
| 24 | `FileChanged` | A **watched** file changes on disk |
| 25 | `WorktreeCreate` | A worktree is being created. **Replaces default git behavior** |
| 26 | `WorktreeRemove` | A worktree is being removed |
| 27 | `PreCompact` | Before context compaction |
| 28 | `PostCompact` | After context compaction |
| 29 | `PreModelSwitch` | Before a requested model switch is applied |
| 30 | `PostModelSwitch` | After the session's model changes |
| 31 | `Elicitation` | An MCP server requests user input during a tool call |
| 32 | `ElicitationResult` | After the user responds to an MCP elicitation |
| — | `SessionEnd` | A session terminates |

(`SessionEnd` is the 33rd; it is listed last in the reference's lifecycle table.)

### 1.1 Verdict on every name the PRD §4.2 lists

**All 17 names in PRD §4.2 exist.** No renames needed. But four carry conditions that change the design:

| PRD name | Exists? | Condition |
|---|---|---|
| `PermissionRequest` | ✅ | Exit 2 **is not honored**. Only a `decision` object can allow/deny. A silent exit-0 hook is safe. |
| `Elicitation` | ✅ | **Exit 2 denies the elicitation.** Exit 0 always. |
| `ElicitationResult` | ✅ | **Exit 2 blocks the response (becomes `decline`).** Exit 0 always. |
| `SubagentStart` | ✅ | Cannot block. Exit-2 stderr renders as a hook-error notice in the *subagent's* transcript. |
| `SubagentStop` | ✅ | **Exit 2 prevents the subagent from stopping.** Exit 0 always. |
| `TaskCreated` | ✅ | **Exit 2 rolls back task creation.** Exit 0 always. |
| `TaskCompleted` | ✅ | **Exit 2 prevents completion.** Exit 0 always. |
| `Stop` | ✅ | **Exit 2 prevents Claude from stopping.** Exit 0 always. |
| `StopFailure` | ✅ | Output and exit code ignored entirely (except `terminalSequence`). Safest event on the list. |
| `TeammateIdle` | ✅ | **Exit 2 prevents the teammate from going idle.** Exit 0 always. |
| `PostToolUseFailure` | ✅ | Cannot block. Exit 2 merely shows stderr to Claude. |
| `WorktreeCreate` | ✅ | 🚨 **DO NOT REGISTER A `polis-hook` HANDLER HERE.** See §9.1. |
| `WorktreeRemove` | ✅ | Cannot block. Failures logged in debug mode only. Safe. |
| `SessionStart` | ✅ | Cannot block. **Plain-text stdout is injected into Claude's context** — the hook must write nothing to stdout. |
| `SessionEnd` | ✅ | Cannot block. **1.5-second budget shared across all SessionEnd hooks.** |
| `PreCompact` | ✅ | **Exit 2 blocks compaction.** Exit 0 always. |
| `PostCompact` | ✅ | Cannot block. Safe. |

### 1.2 Valid events the PRD missed that Polis needs

> **Correction — read §6 and ADR-0044 before acting on this table.** This section
> reads as a list of events to register, and §6's `install-hooks` template
> registers only four of the seven. That is deliberate, not an omission: the code
> is right and this table is the earlier, wider survey.
>
> **Registered:** `PreToolUse` (narrowed to `^(Edit|Write|NotebookEdit)$`),
> `Notification` (narrowed), `CwdChanged`.
> **Rejected:** `FileChanged` — it is not a repo watcher and carries no
> attribution (§9.2, ADR-0003).
> **Deliberately not registered:** `PostToolUse`, `UserPromptSubmit`,
> `PermissionDenied` — each is covered at zero process-spawn cost by Channel A or
> Channel C, and `UserPromptSubmit`'s exit 2 erases the user's prompt.
> ADR-0044 gives the full reasoning per event and the conditions under which each
> would be revisited.
>
> Nothing else in this document is amended; the payload and exit-code facts below
> stand for every event whether Polis registers it or not.

| Event | Why Polis needs it | Caveat |
|---|---|---|
| `PreToolUse` | **PRD §11.3 depends on it** for contention claims on `Edit`/`Write`. §4.2's hook list omits it — an internal PRD contradiction. | Exit 2 blocks the tool call. Exit 0 always. |
| `PostToolUse` | Claim release (close the §11.3 claim when the write lands); `tool_response` on the `Agent` tool carries subagent run telemetry (`agentId`, `totalDurationMs`, `totalToolUseCount`, `usage`). | High frequency if matched with `*`. Match narrowly. |
| `FileChanged` | PRD §4.3 says "fall back to the `FileChanged` hook" for attribution. | 🚨 **It is not a repo watcher.** See §9.2. |
| `Notification` | `permission_prompt` and `idle_prompt` types feed attention state (a) in §11.2, and fire even with desktop notifications disabled. | Cannot block; exit code and stderr ignored. Very safe. |
| `PermissionDenied` | Auto-mode denials are invisible to `PermissionRequest`. Useful for the drill-down layer. | Exit code and stderr ignored. Safe. |
| `CwdChanged` | Tracks `cd` and worktree entry; `old_cwd`/`new_cwd` support §7.6 worktree-prefix stripping. | Cannot block. Safe. |
| `UserPromptSubmit` | Marks the start of a turn; supplies the `prompt_id` correlation key for §4.1 OTel joins. | 🚨 **Exit 2 blocks the prompt and erases it.** Timeout drops to 30s. |

---

## 2. Common input fields (every event)

Delivered as one JSON object on **stdin** for `type: "command"` hooks.

| Field | Type | Notes |
|---|---|---|
| `session_id` | string | Current session identifier. **Stays the parent session's id inside a subagent.** |
| `prompt_id` | string (UUID) | Matches the `prompt.id` attribute on OpenTelemetry events — this is the Channel A ↔ Channel B join key. Absent until first user input. Requires v2.1.196+. |
| `transcript_path` | string | Path to the conversation JSONL. Written asynchronously; **may lag the in-memory conversation**. |
| `cwd` | string | Working directory when the hook is invoked. **Follows Claude into a worktree and after `cd`.** |
| `permission_mode` | string | `"default"` \| `"plan"` \| `"acceptEdits"` \| `"auto"` \| `"dontAsk"` \| `"bypassPermissions"`. The UI's **Manual** mode arrives as `"default"`. Not present on every event. |
| `effort` | object | `{ "level": "low"\|"medium"\|"high"\|"xhigh"\|"max" }`. Present on events inside a tool-use context (`PreToolUse`, `PostToolUse`, `Stop`, `SubagentStop`) when the model supports it. |
| `hook_event_name` | string | Name of the event that fired. |

### 2.1 Subagent discriminator — CRITICAL for Polis thread attribution

Two extra fields appear **only** when running with `--agent` or inside a subagent:

| Field | Type | Notes |
|---|---|---|
| `agent_id` | string | Unique subagent identifier. **Present only when the hook fires inside a subagent call.** The reference states verbatim: *"Use this to distinguish subagent hook calls from main-thread calls."* |
| `agent_type` | string | Agent name, e.g. `"Explore"`, `"Plan"`, `"general-purpose"`, a custom frontmatter `name`, or a plugin-scoped `my-plugin:reviewer`. Present when the session uses `--agent` **or** the hook fires inside a subagent. For subagents, the subagent's type wins over the session's `--agent` value. |

**Polis attribution rule (derived, load-bearing):**

```
thread_id     := session_id                         // the Thread in PRD §3 terms
is_worker     := payload.agent_id.is_some()
worker_id     := payload.agent_id                   // Worker.agent_id in PRD §5
worker_kind   := payload.agent_type
```

Caveat: `agent_type` alone is **not** a worker discriminator — a main agent launched with `claude --agent foo` also carries `agent_type` with no `agent_id`. Key `is_worker` on **`agent_id` presence only**.

### 2.2 Where hooks fire inside subagents

Verified from the reference:

- Tool events (`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest`, `PermissionDenied`) fire inside subagents with the **same configured hooks** as the main conversation.
- Hooks from settings files, managed policy, and plugins all run inside subagents.
- Subagent frontmatter can register its own hooks; a `Stop` hook declared there is converted to `SubagentStop`.
- As of v2.1.198 subagents run in the background by default (`Agent` tool `tool_response.status == "async_launched"`), and `SubagentStart` / `SubagentStop` still fire.

---

## 3. Per-event payloads

Every JSON block below is reproduced verbatim from the reference.

### 3.1 `SessionStart`

Matcher: session start reason — `startup` | `resume` | `clear` | `compact` | `fork`.

Extra input fields: `source` (required), `model` (optional, may be absent after `/clear`), `agent_type` (present with `claude --agent <name>`), `session_title` (optional). When `source` is `"resume"` or `"fork"` and the transcript has at least one Claude response, v2.1.251+ adds `seconds_since_last_response`, `context_tokens`, `prompt_cache_likely_expired`, `estimated_cache_write_usd`.

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "hook_event_name": "SessionStart",
  "source": "resume",
  "model": "claude-opus-5",
  "seconds_since_last_response": 5400,
  "context_tokens": 182340,
  "prompt_cache_likely_expired": true,
  "estimated_cache_write_usd": 1.1396
}
```

⚠️ `SessionStart` is one of four events (with `UserPromptSubmit`, `UserPromptExpansion`, `PostModelSwitch`) where **plain-text stdout is injected into Claude's context**. `polis-hook` must never write to stdout.

Also available here only: the `CLAUDE_ENV_FILE` environment variable (also on `Setup`, `CwdChanged`, `FileChanged`).

### 3.2 `SessionEnd`

Matcher: exit reason — `clear` | `resume` | `logout` | `prompt_input_exit` | `other`.
(`bypass_permissions_disabled` was removed in v2.1.234 — do not put it in a matcher.)

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "hook_event_name": "SessionEnd",
  "reason": "other"
}
```

⚠️ **All `SessionEnd` hooks share a 1.5-second budget.** Setting a longer per-hook `timeout` in a settings file raises the budget to match, capped at 60s. Plugin-provided timeouts do not raise it. `CLAUDE_CODE_SESSIONEND_HOOKS_TIMEOUT_MS` overrides in milliseconds. Polis's hook is ~1ms, so the default budget is ample.

### 3.3 `PreToolUse`

Matcher: **tool name**. Fires for every tool except `EndConversation`.

```json
{
  "session_id": "abc123",
  "prompt_id": "550e8400-e29b-41d4-a716-446655440000",
  "transcript_path": "/home/user/.claude/projects/.../transcript.jsonl",
  "cwd": "/home/user/my-project",
  "permission_mode": "default",
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": {
    "command": "npm test",
    "description": "Run test suite",
    "timeout": 120000,
    "run_in_background": false
  },
  "tool_use_id": "toolu_01ABC123..."
}
```

**Windows path shape** — verbatim from the reference, and directly load-bearing for PRD §7.6 and §11.3:

```json
{
  "hook_event_name": "PreToolUse",
  "tool_name": "Write",
  "tool_input": {
    "file_path": "C:\\project\\src\\index.ts",
    "content": "..."
  },
  ...
}
```

Rules the reference states explicitly:
- For `Write`, `Edit`, and `Read`, `tool_input.file_path` is **always absolute**. `~` and relative paths are expanded before hooks run.
- **On Windows the path arrives with backslash separators**, even when the hook runs under Git Bash where `$PWD` looks like `/c/project`.
- A forward-slash comparison never matches. Normalize separators before comparing, and match a path *segment* rather than anchoring with `^`.

🚨 **`PreToolUse` does not fire for files referenced with `@` in a prompt.** Claude Code inserts their contents while building the prompt — no tool call, so no `Read` hook. This is a real gap in PRD §6.1's evidence stream.

#### `tool_input` shapes by tool

| Tool | Fields |
|---|---|
| `Bash` | `command`, `description?`, `timeout?` (ms), `run_in_background?` |
| `PowerShell` | same as `Bash` |
| `Write` | `file_path`, `content` |
| `Edit` | `file_path`, `old_string`, `new_string`, `replace_all?` |
| `Read` | `file_path`, `offset?`, `limit?` |
| `Glob` | `pattern`, `path?` |
| `Grep` | `pattern`, `path?`, `glob?`, `output_mode?`, `-i?`, `multiline?` |
| `WebFetch` | `url`, `prompt` |
| `WebSearch` | `query`, `allowed_domains?`, `blocked_domains?` |
| `Agent` | `prompt`, `description`, `subagent_type`, `model?` |
| `AskUserQuestion` | `questions`, `answers?` |
| `ExitPlanMode` | `plan`, `planFilePath`, `allowedPrompts` (deprecated) |

🚨 **Windows shell-tool naming:** on Windows where the PowerShell tool is enabled, Claude routes shell commands through `PowerShell`, not `Bash`. **On Windows without Git Bash, Claude Code does not register the `Bash` tool at all.** A matcher of `Bash` alone never fires there. Use `Bash|PowerShell` wherever Polis weights shell activity (PRD §6.1 "Bash cwd", weight 0.5).

### 3.4 `PostToolUse`

Matcher: tool name.

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "permission_mode": "default",
  "hook_event_name": "PostToolUse",
  "tool_name": "Write",
  "tool_input": {
    "file_path": "/path/to/file.txt",
    "content": "file content"
  },
  "tool_response": {
    "filePath": "/path/to/file.txt",
    "success": true
  },
  "tool_use_id": "toolu_01ABC123...",
  "duration_ms": 12
}
```

`duration_ms` is optional; it excludes time in permission prompts and `PreToolUse` hooks.

**`Agent`-tool `tool_response`** (a foreground subagent completing) carries subagent run telemetry, useful for §11.2 "done" bookkeeping:

| Field | Type | Notes |
|---|---|---|
| `status` | string | `"completed"` (foreground) or `"async_launched"` (background — the default since v2.1.198) |
| `agentId` | string | Identifier for the subagent run |
| `content` | array | The subagent's final text blocks |
| `resolvedModel` | string | Model the subagent started on (v2.1.174+) |
| `modelsUsed` | array | Models in order, repeats collapsed (v2.1.212+) |
| `totalTokens` | number | **Final API request only**, not a whole-run total |
| `totalDurationMs` | number | Wall-clock duration of the run |
| `totalToolUseCount` | number | Count of tool calls the subagent made |
| `usage` | object | `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens` — final request only |

A background launch returns immediately with only `status`, `agentId`, `description`, `prompt`, `outputFile`, `resolvedModel`.

### 3.5 `PostToolUseFailure`

Matcher: tool name.

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "permission_mode": "default",
  "hook_event_name": "PostToolUseFailure",
  "tool_name": "Bash",
  "tool_input": {
    "command": "npm test",
    "description": "Run test suite"
  },
  "tool_use_id": "toolu_01ABC123...",
  "error": "Exit code 1\nError: Cannot find module 'express'",
  "is_interrupt": false,
  "duration_ms": 4187
}
```

| Field | Notes |
|---|---|
| `error` | Free-form string. **Not a stable format.** Key on `tool_name`, `is_interrupt`, and the `Exit code N` first line; treat the rest as display text. Middle-truncated past 10,000 chars around a `... [N characters truncated] ...` marker. |
| `is_interrupt` | Optional bool. True when the failure arrived as an abort. **Cancelling a running tool does not fire this hook.** |
| `duration_ms` | Optional. |

Does **not** fire for calls rejected before execution: unknown tool name, schema-validation failure, or a permission denial.

### 3.6 `PermissionRequest`

Matcher: tool name. Receives `tool_name` and `tool_input` like `PreToolUse` **but no `tool_use_id`**.

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "permission_mode": "default",
  "hook_event_name": "PermissionRequest",
  "tool_name": "Bash",
  "tool_input": {
    "command": "rm -rf node_modules",
    "description": "Remove node_modules directory"
  },
  "permission_suggestions": [
    {
      "type": "addRules",
      "rules": [{ "toolName": "Bash", "ruleContent": "rm -rf node_modules" }],
      "behavior": "allow",
      "destination": "localSettings"
    }
  ]
}
```

Fires the instant Claude asks for permission. The `Notification` hook with type `permission_prompt` fires only **after the prompt has waited about six seconds** — so `PermissionRequest` is the low-latency source for PRD §11.2 attention state (a).

Not fired for a sandboxed command's network-request prompt; use the `permission_prompt` notification type for that.

### 3.7 `PermissionDenied`

Matcher: tool name. Fires only in **auto mode**.

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "permission_mode": "auto",
  "hook_event_name": "PermissionDenied",
  "tool_name": "Bash",
  "tool_input": {
    "command": "rm -rf /tmp/build",
    "description": "Clean build directory"
  },
  "tool_use_id": "toolu_01ABC123...",
  "reason": "Blocked by classifier"
}
```

### 3.8 `SubagentStart`

Matcher: **agent type**. For plugin subagents the type is plugin-scoped (`my-plugin:reviewer`); the colon puts it on the regex path, so anchor as `^my-plugin:reviewer$`.

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "hook_event_name": "SubagentStart",
  "agent_id": "agent-abc123",
  "agent_type": "Explore"
}
```

Note there is **no `permission_mode`** on this event.

### 3.9 `SubagentStop`

Matcher: agent type, same values as `SubagentStart`.

```json
{
  "session_id": "abc123",
  "transcript_path": "~/.claude/projects/.../abc123.jsonl",
  "cwd": "/Users/...",
  "permission_mode": "default",
  "hook_event_name": "SubagentStop",
  "stop_hook_active": false,
  "agent_id": "def456",
  "agent_type": "Explore",
  "agent_transcript_path": "~/.claude/projects/.../abc123/subagents/agent-def456.jsonl",
  "last_assistant_message": "Analysis complete. Found 3 potential issues...",
  "background_tasks": [],
  "session_crons": []
}
```

🚨 **`transcript_path` is the *main session's* transcript; `agent_transcript_path` is the subagent's own, in a nested `subagents/` folder.** PRD §4.4 models only the flat `<session-id>.jsonl` layout and misses this. See §9.4.

`background_tasks` and `session_crons` are scoped to the **parent** session, not the subagent. Their shapes are documented under `Stop` (§3.11).

### 3.10 `TaskCreated` / `TaskCompleted`

**No matcher support** — they fire on every occurrence.

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "hook_event_name": "TaskCreated",
  "task_id": "task-001",
  "task_subject": "Implement user authentication",
  "task_description": "Add login and signup endpoints",
  "teammate_name": "implementer",
  "team_name": "session-a1b2c3d4"
}
```

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "permission_mode": "default",
  "hook_event_name": "TaskCompleted",
  "task_id": "task-001",
  "task_subject": "Implement user authentication",
  "task_description": "Add login and signup endpoints",
  "teammate_name": "implementer",
  "team_name": "session-a1b2c3d4"
}
```

`task_description`, `teammate_name` may be absent. `team_name` is **deprecated** and will be removed — do not key Polis state on it.

In a session without the Task tools, `TaskCreated` never fires.

### 3.11 `Stop`

**No matcher support.** Does not run on user interrupt; API errors fire `StopFailure` instead.

```json
{
  "session_id": "abc123",
  "transcript_path": "~/.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "permission_mode": "default",
  "hook_event_name": "Stop",
  "stop_hook_active": true,
  "last_assistant_message": "I've completed the refactoring. Here's a summary...",
  "background_tasks": [
    {
      "id": "task-001",
      "type": "shell",
      "status": "running",
      "description": "tail logs",
      "command": "tail -f /var/log/syslog"
    }
  ],
  "session_crons": [
    {
      "id": "cron-001",
      "schedule": "0 9 * * 1-5",
      "recurring": true,
      "prompt": "check the build"
    }
  ]
}
```

`background_tasks[]` fields: `id`, `type` (`shell` | `subagent` | `monitor` | `workflow` | `teammate` | `cloud session` | `MCP task`), `status`, `description`, and conditionally `command` (shell), `agent_type` (subagent), `server` / `tool` (monitor, MCP task), `name` (workflow).

`session_crons[]` fields: `id`, `schedule`, `recurring`, `prompt`.

**Directly useful for PRD §11.2(b) "Done".** The reference states the arrays *"let hooks distinguish 'session is done' from 'session is paused waiting for background work to wake it back up'."* Both arrays are present (possibly empty) when the task registry is reachable. Polis should render a thread with non-empty `background_tasks` as **Working**, not **Done**.

`stop_hook_active` is `true` when Claude Code is already continuing because of a stop hook. Claude Code overrides the hook and ends the turn after **8 consecutive blocks**.

### 3.12 `StopFailure`

Matcher: **error type** — `rate_limit` | `overloaded` | `authentication_failed` | `oauth_org_not_allowed` | `account_on_hold` | `billing_error` | `invalid_request` | `model_not_found` | `server_error` | `max_output_tokens` | `unknown`.

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "hook_event_name": "StopFailure",
  "error": "rate_limit",
  "error_details": "429 Too Many Requests",
  "last_assistant_message": "API Error: Rate limit reached"
}
```

⚠️ `last_assistant_message` here holds the **API error string**, not Claude's conversational output — unlike `Stop` and `SubagentStop`.

Note `StopFailure` uses the **narrow** matcher character set (letters, digits, `_`, `|` only) — see §5.2.

### 3.13 `TeammateIdle`

**No matcher support.**

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "permission_mode": "default",
  "hook_event_name": "TeammateIdle",
  "teammate_name": "researcher",
  "team_name": "session-a1b2c3d4"
}
```

`team_name` is deprecated.

### 3.14 `Notification`

Matcher: notification type. Full set: `permission_prompt`, `idle_prompt`, `auth_success`, `elicitation_dialog`, `elicitation_url_dialog`, `elicitation_complete`, `elicitation_response`, `agent_needs_input`, `agent_completed`, `quota_auto_resume_fired`, `quota_auto_resume_stale`, `quota_auto_resume_disabled`.

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "hook_event_name": "Notification",
  "message": "Claude needs your permission",
  "title": "Permission needed",
  "notification_type": "permission_prompt"
}
```

Fires **even when desktop notifications are turned off** — `preferredNotifChannel` (including `notifications_disabled`) changes only how the user is alerted, not whether the hook runs. Cannot block; exit code and stderr are ignored. This is the single safest event to register.

### 3.15 `FileChanged`

Matcher: **literal filenames to watch**, split on `|`. Regex is useless here — `^\.env` would watch a file literally named `^\.env`.

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../transcript.jsonl",
  "cwd": "/Users/my-project",
  "hook_event_name": "FileChanged",
  "file_path": "/Users/my-project/.envrc",
  "event": "change"
}
```

`event` is `"change"` | `"add"` | `"unlink"`.

🚨 See §9.2 — this cannot replace `notify`.

### 3.16 `WorktreeCreate`

**No matcher support.**

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "hook_event_name": "WorktreeCreate",
  "name": "feature-auth"
}
```

`name` is a slug, e.g. `bold-oak-a3f2`. 🚨 See §9.1 — **Polis must not register here.**

### 3.17 `WorktreeRemove`

**No matcher support.** Safe.

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "hook_event_name": "WorktreeRemove",
  "worktree_path": "/Users/.../my-project/.claude/worktrees/feature-auth"
}
```

`worktree_path` is the path `WorktreeCreate` returned. Failures logged in debug mode only.

### 3.18 `PreCompact` / `PostCompact`

Matcher: `manual` | `auto`.

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "hook_event_name": "PreCompact",
  "trigger": "manual",
  "custom_instructions": null
}
```

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "hook_event_name": "PostCompact",
  "trigger": "manual",
  "compact_summary": "Summary of the compacted conversation..."
}
```

`custom_instructions` is `null` for `auto` and for a bare `/compact`. ⚠️ **`PreCompact` exit 2 blocks compaction.**

### 3.19 `Elicitation` / `ElicitationResult`

Matcher: **MCP server name**.

Form mode:

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "permission_mode": "default",
  "hook_event_name": "Elicitation",
  "mcp_server_name": "my-mcp-server",
  "message": "Please provide your credentials",
  "mode": "form",
  "requested_schema": {
    "type": "object",
    "properties": {
      "username": { "type": "string", "title": "Username" }
    }
  }
}
```

URL mode:

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "permission_mode": "default",
  "hook_event_name": "Elicitation",
  "mcp_server_name": "my-mcp-server",
  "message": "Please authenticate",
  "mode": "url",
  "url": "https://auth.example.com/login"
}
```

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
  "cwd": "/Users/...",
  "permission_mode": "default",
  "hook_event_name": "ElicitationResult",
  "mcp_server_name": "my-mcp-server",
  "action": "accept",
  "content": { "username": "alice" },
  "mode": "form",
  "elicitation_id": "elicit-123"
}
```

⚠️ Exit 2 on `Elicitation` **denies** it; exit 2 on `ElicitationResult` **forces `decline`**. Also: on both events an exit-2 hook's `hookSpecificOutput` is ignored.

### 3.20 `CwdChanged` (bonus — supports §7.6)

**No matcher support.**

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/.../.claude/projects/.../transcript.jsonl",
  "cwd": "/Users/my-project/src",
  "hook_event_name": "CwdChanged",
  "old_cwd": "/Users/my-project",
  "new_cwd": "/Users/my-project/src"
}
```

---

## 4. Exit-code contract

### 4.1 Exit 0

- Success. The intended code when printing JSON for structured control.
- **stdout**: for most events, written to the debug log and never shown to Claude. **Exceptions — `UserPromptSubmit`, `UserPromptExpansion`, `SessionStart`, `PostModelSwitch` — add plain-text stdout as context Claude can see and act on.**
- Parsing rule, ignoring surrounding whitespace: stdout that **starts with `{` and ends with `}`** is parsed as JSON; starts with `{` but does not end with `}` → plain text; starts with anything else (including a JSON array or quoted string) → plain text.
- Since v2.1.248, stdout that *looks* like JSON but fails to parse is a **non-blocking error** on every exit code except 2, and on the context-injecting events the text is **not** added. (Before v2.1.248 it was treated as plain text.)
- **stderr**: debug log only. Never the transcript. Claude never sees it.

### 4.2 Exit 2 — blocking error

- On events that can block, exit 2 blocks **whether or not** JSON is printed. A JSON `permissionDecision: "allow"` cannot override it.
- The blocking message is the reason from a JSON blocking decision if present, otherwise **stderr**.
- A hook that exits 2 while printing schema-invalid JSON **still blocks**, using stderr as the reason (changed in v2.1.214).

### 4.3 Exit 1 and everything else

- **Does not block** for most events. This is the trap: exit 1 is the conventional Unix failure code but Claude Code treats it as a non-blocking error and proceeds.
- Valid, schema-passing JSON on stdout → the exit code is ignored and the JSON alone decides.
- Schema-invalid or unparseable JSON, plain text, or empty stdout → non-blocking error; the transcript shows `<hook name> hook error` followed by the first line of stderr prefixed `Failed with non-blocking status code:`.
- A hook that cannot start (bad path, not executable) lands in the same bucket, e.g. `Failed with non-blocking status code: /bin/sh: /path/to/hook.sh: No such file or directory`. **Watch for this notice on the first run after `polis install-hooks` — a mistyped path leaves the channel silently dead.**
- 🚨 **Exception: `WorktreeCreate` — any non-zero exit aborts worktree creation.**

### 4.4 Exit code 2 behavior, per event (verbatim table)

| Hook event | Can block? | What happens on exit 2 |
|---|---|---|
| `PreToolUse` | **Yes** | Blocks the tool call |
| `PermissionRequest` | No | Not honored; permission flow proceeds unchanged. Deny via the `decision` object instead |
| `UserPromptSubmit` | **Yes** | Blocks prompt processing and erases the prompt |
| `UserPromptExpansion` | **Yes** | Blocks the expansion |
| `Stop` | **Yes** | Prevents Claude from stopping, continues the conversation |
| `SubagentStop` | **Yes** | Prevents the subagent from stopping |
| `TeammateIdle` | **Yes** | Prevents the teammate from going idle |
| `TaskCreated` | **Yes** | Rolls back the task creation |
| `TaskCompleted` | **Yes** | Prevents the task from being marked completed |
| `ConfigChange` | **Yes** | Blocks the config change (except `policy_settings`) |
| `StopFailure` | No | Output and exit code ignored, except `terminalSequence` |
| `PostToolUse` | No | Shows stderr to Claude; the tool already ran |
| `PostToolUseFailure` | No | Shows stderr to Claude; the tool already failed |
| `PostToolBatch` | **Yes** | Stops the agentic loop before the next model call |
| `PermissionDenied` | No | Exit code and stderr ignored |
| `Notification` | No | Exit code and stderr ignored |
| `SubagentStart` | No | Shows stderr to user only |
| `SessionStart` | No | Shows stderr to user only |
| `Setup` | No | Exit code and stderr ignored |
| `SessionEnd` | No | Shows stderr to user only |
| `CwdChanged` | No | Shows stderr to user only |
| `DirectoryAdded` | No | Stderr to debug log; the directory is already added |
| `FileChanged` | No | Shows stderr to user only |
| `PreCompact` | **Yes** | Blocks compaction |
| `PostCompact` | No | Shows stderr to user only |
| `PreModelSwitch` | **Yes** | Blocks the model switch |
| `PostModelSwitch` | No | Shows stderr to user only |
| `Elicitation` | **Yes** | Denies the elicitation |
| `ElicitationResult` | **Yes** | Blocks the response (action becomes `decline`) |
| `WorktreeCreate` | **Yes** | **Any non-zero exit code causes worktree creation to fail** |
| `WorktreeRemove` | No | Failures logged in debug mode only |
| `InstructionsLoaded` | No | Exit code is ignored |
| `MessageDisplay` | No | The original text is displayed |

**PRD §4.2's "never exit non-zero, exit 0 always" rule is confirmed correct and non-negotiable.** Eleven of the events Polis registers can block an agent on exit 2.

### 4.5 JSON output protocol

`polis-hook` prints nothing, so this is reference material only.

Universal fields (accepted by every event; some events discard them):

| Field | Default | Description |
|---|---|---|
| `continue` | `true` | If `false`, Claude stops processing entirely. Takes precedence over event-specific decision fields |
| `stopReason` | none | Shown to the user when `continue` is `false`. Not shown to Claude |
| `suppressOutput` | `false` | 🚨 **Has no effect.** Claude Code accepts the field but does not act on it. Successful-hook stdout is never shown in the transcript anyway |
| `systemMessage` | none | Warning shown to the user. Can arrive as `SDKInformationalMessage` in stream-json output |
| `terminalSequence` | none | Terminal escape sequence Claude Code emits on the hook's behalf. Restricted to OSC `0`/`1`/`2`/`9`/`99`/`777` and BEL; anything else and the field is ignored. Works even on events that discard `systemMessage` |

Two other shapes: top-level `decision` + `reason` (used by `UserPromptSubmit`, `UserPromptExpansion`, `PostToolUse`, `PostToolUseFailure`, `PostToolBatch`, `Stop`, `SubagentStop`, `ConfigChange`, `PreCompact`), and the nested `hookSpecificOutput` object, which requires a `hookEventName` field set to the event name.

`PreToolUse` uses `hookSpecificOutput.permissionDecision` (`"allow"` | `"deny"` | `"ask"` | `"defer"`), `permissionDecisionReason`, `updatedInput`, `additionalContext`. Multi-hook precedence: **`deny` > `defer` > `ask` > `allow`**. Top-level `decision`/`reason` are deprecated for this event.

All hook output strings (`additionalContext`, `systemMessage`, plain stdout) are capped at **10,000 characters**; longer output is saved to a file and replaced by a preview plus the path.

---

## 5. `settings.json` configuration format

### 5.1 Nesting

Three levels, exactly as the PRD guessed:

```
hooks
 └─ "<EventName>"          : array of matcher groups
     └─ { matcher?, hooks: [ handler, ... ] }
         └─ handler        : { type, ... }
```

Top-level `"disableAllHooks": true` disables everything without removing it. It respects settings precedence, so a project `false` overrides a user `true`; `--settings '{"disableAllHooks": true}'` wins over both.

**Locations, in scope order:**

| Location | Scope | Shareable |
|---|---|---|
| `~/.claude/settings.json` | All projects | No |
| `.claude/settings.json` | Single project | Yes |
| `.claude/settings.local.json` | Single project | No (gitignored) |
| Managed policy settings | Organization-wide | Yes |
| Plugin `hooks/hooks.json` | When plugin enabled | Yes |
| Skill frontmatter | Rest of session once invoked | Yes |
| Subagent frontmatter | While that subagent runs | Yes |

Plugin `hooks/hooks.json` additionally accepts a top-level `description` string.

### 5.2 Matcher evaluation

| Matcher value | Evaluated as |
|---|---|
| `"*"`, `""`, or omitted | Match all |
| Only letters, digits, `_`, `-`, spaces, `,`, `\|` | Exact string, or a list separated by `\|` or `,` with optional surrounding whitespace |
| Contains any other character | **JavaScript regular expression, unanchored** (`RegExp.prototype.test`) |

Consequences that will bite:
- `Edit.*` matches both `Edit` **and** `NotebookEdit`. Anchor with `^Edit$` for a whole-string match.
- `mcp__memory` (exact-match chars only) matches **no tool**. You must write `mcp__memory__.*`.
- Comma separators and whitespace tolerance require **v2.1.191+**; hyphens in the exact-match set require **v2.1.195+**.
- `FileChanged` and `StopFailure` use a **narrower** exact-match set: letters, digits, `_`, and `|` only. A hyphen, space, or comma there stays on the regex path, and only `|` separates alternatives.
- A `matcher` on an event without matcher support is **silently ignored**.

**What each event matches against:**

| Event(s) | Matcher filters |
|---|---|
| `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest`, `PermissionDenied` | tool name |
| `SessionStart` | `startup` / `resume` / `clear` / `compact` / `fork` |
| `Setup` | `init` / `maintenance` |
| `SessionEnd` | `clear` / `resume` / `logout` / `prompt_input_exit` / `other` |
| `Notification` | notification type |
| `SubagentStart`, `SubagentStop` | agent type |
| `PreCompact`, `PostCompact` | `manual` / `auto` |
| `PreModelSwitch`, `PostModelSwitch` | canonical model name derived from `to_model` |
| `ConfigChange` | `user_settings` / `project_settings` / `local_settings` / `policy_settings` / `skills` |
| `DirectoryAdded` | `slash_command` / `register_repo_root` |
| `FileChanged` | literal filenames to watch |
| `StopFailure` | error type |
| `InstructionsLoaded` | `session_start` / `nested_traversal` / `path_glob_match` / `include` / `compact` |
| `UserPromptExpansion` | command name |
| `Elicitation`, `ElicitationResult` | MCP server name |
| **`CwdChanged`, `UserPromptSubmit`, `PostToolBatch`, `Stop`, `TeammateIdle`, `TaskCreated`, `TaskCompleted`, `WorktreeCreate`, `WorktreeRemove`, `MessageDisplay`** | **no matcher support — always fires** |

### 5.3 Handler fields

Five handler types: `command`, `http`, `mcp_tool`, `prompt`, `agent`. Polis uses `command` only.

Common fields:

| Field | Required | Notes |
|---|---|---|
| `type` | yes | one of the five |
| `if` | no | One permission rule, e.g. `"Bash(git *)"`, `"Edit(*.ts)"`. **Only evaluated on tool events.** On other events, a handler with `if` set **never runs.** No `&&`/`\|\|`/list syntax. Best-effort only |
| `timeout` | no | Seconds. Defaults: **600** for `command`/`http`/`mcp_tool`, 30 for `prompt`, 60 for `agent`. Lowered to 30 on `UserPromptSubmit`/`PreModelSwitch`/`PostModelSwitch`, 10 on `MessageDisplay`. `SessionEnd` shares a 1.5s budget |
| `statusMessage` | no | Custom spinner message |
| `once` | no | Remove after first successful run. **Only honored in skill frontmatter** |

Command-hook fields:

| Field | Notes |
|---|---|
| `command` | Shell command, or (with `args`) the executable to spawn directly |
| `args` | Argument list. **When present, no shell is involved** — `command` is resolved on `PATH` and spawned directly; each element is one argument verbatim; path placeholders substitute as plain strings |
| `async` | Background, non-blocking. `timeout` not enforced once running |
| `asyncRewake` | Background; wakes Claude on exit 2. `timeout` **is** enforced |
| `shell` | `"bash"` or `"powershell"`. Defaults to `bash`, or `powershell` on Windows when Git Bash is absent. Ignored when `args` is set |

**Exec form vs shell form** — decisive for Polis:
- `args` present → **exec form**, no shell. One less process, no tokenization, special characters pass through verbatim.
- `args` absent → **shell form**: `sh -c` on macOS/Linux, Git Bash on Windows, or PowerShell when Git Bash is absent.
- On Windows, exec form requires `command` to resolve to a real executable such as a `.exe`. `.cmd`/`.bat` shims cannot be spawned without a shell. **`polis-hook.exe` is a real executable, so exec form applies and is the right choice.**

### 5.4 Environment available to a hook process

Path placeholders, substituted in `command` and each `args` element, **and** exported as environment variables on the spawned process:

| Name | Meaning |
|---|---|
| `CLAUDE_PROJECT_DIR` | **The project root where the session started.** Also set for stdio MCP servers and plugin LSP servers |
| `CLAUDE_PLUGIN_ROOT` | Plugin installation directory; changes on every plugin update |
| `CLAUDE_PLUGIN_DATA` | Plugin persistent data directory |

Environment variables (not placeholders):

| Name | Meaning |
|---|---|
| `CLAUDE_EFFORT` | `low` / `medium` / `high` / `xhigh` / `max` |
| `CLAUDE_CODE_REMOTE` | `"true"` in remote web environments; unset locally |
| `CLAUDE_CODE_BRIDGE_SESSION_ID` | Remote Control session id (v2.1.199+) |
| `CLAUDE_PLUGIN_OPTION_<KEY>` | Plugin option values |
| `CLAUDE_ENV_FILE` | **Only** on `SessionStart`, `Setup`, `CwdChanged`, `FileChanged` |

There is **no `$CLAUDE_MODEL`**.

🚨 **Worktrees (verbatim from the reference), directly load-bearing for PRD §7.6:**
> `${CLAUDE_PROJECT_DIR}` **stays put**: it still points at the project root where the session started […]
> `cwd` **follows Claude**: the `cwd` field in the hook's input JSON is the worktree root after Claude enters a worktree, and the new directory after Claude runs `cd`.

This gives Polis the worktree-prefix strip for free in the daemon: `logical_path = strip_prefix(tool_input.file_path, payload.cwd_worktree_root)`, with `CLAUDE_PROJECT_DIR` as the stable base-map key.

**Inherited environment, minus:** all `OTEL_*` exporter variables, which Claude Code removes from every subprocess it spawns. Additional variables are stripped when `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=1`. Polis's Channel A config (PRD §4.1) is therefore **invisible to the hook** — the hook must not try to read `OTEL_EXPORTER_OTLP_ENDPOINT` to find the daemon.

**Working directory:** handlers run in the current directory. If it no longer exists, Claude Code falls back to the session start directory → project root → home → system temp, and records a warning in the debug log.

### 5.5 Timeouts and parallelism

- **All matching hooks run in parallel.**
- The same handler defined in more than one settings file runs **once**. A plugin's or skill's copy of the same handler stays separate and runs independently.
- A hook cancelled at its timeout has its output discarded, so on most events it renders no decision.
- On `PreToolUse`, a timed-out `command`/`http`/`mcp_tool` hook **does not block** — the call continues through the normal permission flow. (Only an Agent SDK callback hook blocks on timeout.)
- On `PreModelSwitch`, a timed-out hook **does** block the switch.

### 5.6 Workspace trust

- **Interactive session**: Claude Code holds back hooks from *every* settings file — including `~/.claude/settings.json` — until the workspace trust dialog is accepted for the folder or a parent whose trust extends to it.
- **`-p` / SDK session**: the folder is treated as trusted with no dialog; hooks committed in a repo's `.claude/settings.json` run in a folder never trusted.
- Project **subagent frontmatter** hooks run only after the trust dialog is accepted (v2.1.218+). Skill frontmatter hooks follow the settings-file rule.

`allowManagedHooksOnly` blocks user, project, local, and plugin hooks entirely.

### 5.7 Debugging

`claude --debug-file <path>` writes the log to a known location; `claude --debug` writes to `~/.claude/debug/<session-id>.txt` (neither prints to the terminal). Set `CLAUDE_CODE_DEBUG_LOG_LEVEL=verbose` for matcher counts and query matching. `/hooks` opens a read-only browser showing every configured hook with its source (`User Settings`, `Project Settings`, `Local Settings`, `Plugin Hooks`, `Session Hooks`).

---

## 6. The `polis install-hooks` template

Design decisions, each justified by a fact above:

1. **Exec form** (`"args": [...]`) — no shell process, no tokenization, path placeholders substitute as plain strings. `polis-hook.exe` is a real executable so this works on Windows. (§5.3)
2. **The event name is passed as an argv flag.** PRD §4.2 step 2 requires an 8-byte header with a u32 event-kind tag but never says how the hook learns the tag, and §4.2 also forbids JSON parsing in the hook. Exec-form `args` resolves this at zero cost: the hook maps a static `&str` to a `u32` with a `match`. The daemon must still treat `hook_event_name` in the payload as authoritative and the argv tag as a hint.
3. **Explicit `"timeout": 5`.** The 600-second default means a wedged hook occupies a slot for ten minutes. Five seconds is 400× the measured p95.
4. **No `WorktreeCreate` entry.** Registering there breaks worktree creation. (§9.1)
5. **No `FileChanged` entry.** Its matcher is a literal watch list, not a repo watcher. (§9.2)
6. **`PreToolUse` narrowed to `^(Edit|Write|NotebookEdit)$`** — the §11.3 contention claim needs it, but an unmatched `PreToolUse` would be exactly the firehose PRD §4.2 forbids. Anchored because unanchored `Edit` on the regex path also matches `NotebookEdit`. Verified against `code.claude.com/docs/en/tools-reference.md`: the file-mutating built-ins are exactly `Edit`, `Write`, and `NotebookEdit`. **There is no `MultiEdit` tool** — do not put it in the matcher.
7. **No `PostToolUse` entry by default.** Claim release is better driven by the 30s TTL in §11.3 plus Channel A. Add it later only if TTL churn proves insufficient.
8. **`Notification` is included** with the three types that map to attention states, since it cannot block and its exit code is ignored.

Replace `{{POLIS_HOOK_PATH}}` with the absolute path to the installed binary — on Windows, `C:\\Users\\<user>\\.polis\\bin\\polis-hook.exe` with **escaped backslashes**, since this is JSON.

```json
{
  "hooks": {
    "SessionStart": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "SessionStart"], "timeout": 5 } ] }
    ],
    "SessionEnd": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "SessionEnd"], "timeout": 5 } ] }
    ],
    "PreToolUse": [
      {
        "matcher": "^(Edit|Write|NotebookEdit)$",
        "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "PreToolUse"], "timeout": 5 } ]
      }
    ],
    "PostToolUseFailure": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "PostToolUseFailure"], "timeout": 5 } ] }
    ],
    "PermissionRequest": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "PermissionRequest"], "timeout": 5 } ] }
    ],
    "SubagentStart": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "SubagentStart"], "timeout": 5 } ] }
    ],
    "SubagentStop": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "SubagentStop"], "timeout": 5 } ] }
    ],
    "TaskCreated": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "TaskCreated"], "timeout": 5 } ] }
    ],
    "TaskCompleted": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "TaskCompleted"], "timeout": 5 } ] }
    ],
    "Stop": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "Stop"], "timeout": 5 } ] }
    ],
    "StopFailure": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "StopFailure"], "timeout": 5 } ] }
    ],
    "TeammateIdle": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "TeammateIdle"], "timeout": 5 } ] }
    ],
    "Notification": [
      {
        "matcher": "permission_prompt|idle_prompt|agent_needs_input",
        "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "Notification"], "timeout": 5 } ]
      }
    ],
    "Elicitation": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "Elicitation"], "timeout": 5 } ] }
    ],
    "ElicitationResult": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "ElicitationResult"], "timeout": 5 } ] }
    ],
    "WorktreeRemove": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "WorktreeRemove"], "timeout": 5 } ] }
    ],
    "CwdChanged": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "CwdChanged"], "timeout": 5 } ] }
    ],
    "PreCompact": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "PreCompact"], "timeout": 5 } ] }
    ],
    "PostCompact": [
      { "hooks": [ { "type": "command", "command": "{{POLIS_HOOK_PATH}}", "args": ["--event", "PostCompact"], "timeout": 5 } ] }
    ]
  }
}
```

**19 event registrations.** `install-hooks` must merge into an existing `hooks` object key-by-key and warn before replacing any array Polis did not write, per PRD §4.2.

---

## 7. Measured on this machine

Probe project: `C:\Users\konka\AppData\Local\Temp\claude\C--coding-agentolis\6f51089f-1ec0-4e78-9bc9-8ff6864e0100\scratchpad\hookprobe`.
Release profile: `opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, `strip = true`. Zero dependencies.

| Measurement | Result |
|---|---|
| Stripped binary size | **132,608 bytes** |
| In-process work (stdin read → framed → `send_to` → exit), cold | **1.20 ms** |
| End-to-end spawn→exit, 50 samples after 10 warmup, .NET `Process.Start` with redirected stdio | **min 11.05 ms, median 11.92 ms, p95 14.57 ms, max 14.64 ms** |
| `UdpSocket::send_to` to `127.0.0.1:47913` with **no listener bound** | `Ok(152)` — succeeds, does not block, does not error |
| `AF_UNIX` + `SOCK_STREAM` on Windows | Socket created OK |
| `AF_UNIX` + `SOCK_DGRAM` on Windows | **FAILS** — `WSAEAFNOSUPPORT`, "An address incompatible with the requested protocol was used" |
| `$XDG_RUNTIME_DIR` | **unset** |

---

## 8. Reference hook implementation (compiles, runs, verified above)

```rust
use std::io::Read;
use std::net::UdpSocket;

fn main() {
    // Event kind from argv (--event <Name>); never parse the JSON here.
    let tag: u32 = std::env::args()
        .skip_while(|a| a != "--event")
        .nth(1)
        .as_deref()
        .map(event_tag)
        .unwrap_or(0);

    // 1. read stdin, hard-capped at 256 KiB, truncating beyond.
    let mut buf = Vec::with_capacity(64 * 1024);
    let _ = std::io::stdin().take(256 * 1024).read_to_end(&mut buf);

    // 2. 8-byte header: u32 event-kind tag (LE) + u32 payload length (LE).
    let mut frame = Vec::with_capacity(8 + buf.len());
    frame.extend_from_slice(&tag.to_le_bytes());
    frame.extend_from_slice(&(buf.len() as u32).to_le_bytes());
    frame.extend_from_slice(&buf);

    // 3. one non-blocking send on a loopback UDP datagram socket.
    //    With no listener bound this returns Ok(n) on Windows; it never blocks.
    let _ = (|| -> std::io::Result<usize> {
        let s = UdpSocket::bind(("127.0.0.1", 0))?;
        s.set_nonblocking(true)?;
        s.send_to(&frame, ("127.0.0.1", 47913))
    })();

    // 4. exit(0) unconditionally. Any other code can cancel a real agent's work.
    std::process::exit(0);
}

fn event_tag(name: &str) -> u32 {
    match name {
        "SessionStart" => 1,
        "SessionEnd" => 2,
        "PreToolUse" => 3,
        "PostToolUseFailure" => 4,
        "PermissionRequest" => 5,
        "SubagentStart" => 6,
        "SubagentStop" => 7,
        "TaskCreated" => 8,
        "TaskCompleted" => 9,
        "Stop" => 10,
        "StopFailure" => 11,
        "TeammateIdle" => 12,
        "Notification" => 13,
        "Elicitation" => 14,
        "ElicitationResult" => 15,
        "WorktreeRemove" => 16,
        "CwdChanged" => 17,
        "PreCompact" => 18,
        "PostCompact" => 19,
        _ => 0,
    }
}
```

`Cargo.toml` for `polis-hook` — no dependencies at all, so PRD §14's "no deps beyond libc + std" is satisfiable with **std alone**:

```toml
[package]
name = "polis-hook"
version = "0.1.0"
edition = "2021"

[dependencies]

[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
panic = "abort"
strip = true
```

Notes for the implementer:
- Use **unconnected** `send_to`, not `connect()` + `send()`. On Windows a *connected* UDP socket can receive `WSAECONNRESET` on a later send after an ICMP port-unreachable from a dead listener; unconnected `send_to` does not.
- Bind an **ephemeral** source port (`127.0.0.1:0`). Binding a fixed source port makes concurrent hook processes collide.
- 152 bytes was the framed size of a realistic `PreToolUse` payload. Real payloads run larger; a UDP datagram on loopback is fine well past 8 KiB, but PRD's 256 KiB cap exceeds the practical datagram limit (65,507 bytes). **Cap the send at 60 KiB and set a truncation flag in the header**, or the `send_to` fails with `EMSGSIZE` and the event is lost silently.

---

## 9. PRD corrections

### 9.1 🚨 `WorktreeCreate` will break every worktree on the machine

**PRD §4.2 lists `WorktreeCreate` among the events `polis-hook` registers.** The reference states:

> Configuring a WorktreeCreate hook **replaces that default git behavior** […] The hook **must return the path to the created worktree directory**. […] If the hook fails or produces no path, worktree creation fails with an error.

and, in the exit-code table:

> `WorktreeCreate` — Can block: **Yes** — Any non-zero exit code causes worktree creation to fail.

`polis-hook` exits 0 and prints nothing to stdout. Registering it on `WorktreeCreate` therefore **suppresses `git worktree` entirely and then fails creation for want of a path**. Every `claude --worktree` session, every subagent with `isolation: "worktree"`, and every background session on the machine breaks — and PRD §7.6 makes worktrees a first-class feature.

**Remove `WorktreeCreate` from §4.2's list.** Worktree births can be observed through `SessionStart` + `cwd`, or `CwdChanged`, or the `notify` watcher. `WorktreeRemove` is safe and stays.

### 9.2 🚨 `FileChanged` cannot serve as the §4.3 attribution fallback

**PRD §4.3** says: *"Where attribution must be certain (contention detection), fall back to the `FileChanged` hook or `PreToolUse` claims."*

The event exists, but two facts kill that use:

1. **It only watches a literal, explicitly named list.** The reference: *"the value is split on `|` and each segment is registered as a literal filename in the working directory […] Regex patterns are not useful here."* Watching a whole repository would require enumerating every file into a matcher or feeding `watchPaths` — and `watchPaths` can only be returned as **JSON on stdout**, which `polis-hook` is forbidden from producing (PRD §4.2: "No JSON parsing in the hook"; and stdout JSON on `SessionStart` is context-injecting).
2. **It carries no attribution.** The payload is `{ file_path, event }` plus common fields — no `tool_name`, no `tool_use_id`. It fires *"no matter what changed the file: an `Edit` or `Write` tool call, a script Claude runs with `Bash`, or a process outside Claude Code entirely."* It is exactly as attribution-blind as the `notify` watcher it was meant to backstop, while additionally costing a process spawn.

**Correction:** delete "the `FileChanged` hook or" from §4.3. `PreToolUse` claims are the only authoritative attribution channel, which is what §11.3 already specifies.

### 9.3 🚨 §4.2's event list omits `PreToolUse`, which §11.3 requires

§4.2 enumerates the events `polis-hook` registers and `PreToolUse` is absent. §11.3 opens: *"On `PreToolUse` for `Edit`/`Write`, register a claim on the logical path."* The document contradicts itself.

**Correction:** add `PreToolUse` to §4.2's list, **narrowed by matcher** to file-mutating tools (`^(Edit|Write|NotebookEdit)$` — verified complete against the tools reference; there is no `MultiEdit`). Note explicitly that an unmatched `PreToolUse` registration is the ~200-call/sec firehose §4.2 exists to avoid; the matcher is what keeps it rare.

### 9.4 🚨 Unix datagram socket at `$XDG_RUNTIME_DIR/polis.sock` is impossible on Windows

**PRD §4.2 step 3** mandates *"One non-blocking `sendto()` on a Unix **datagram** socket at `$XDG_RUNTIME_DIR/polis.sock`"*, and requires `SOCK_DGRAM` + `O_NONBLOCK`.

Measured on the target machine:
- `AF_UNIX` + `SOCK_DGRAM` → **fails at socket creation** with `WSAEAFNOSUPPORT`. Windows' AF_UNIX support is **stream-only**; `SOCK_DGRAM` was never implemented.
- `$XDG_RUNTIME_DIR` is **unset** on Windows.

**Correction:** replace with a **loopback UDP datagram socket** (`127.0.0.1:<port>`), which preserves every property the PRD actually requires — connectionless, non-blocking, lossy-by-design, never blocks the agent, no listener needed. Verified: `send_to` with no listener bound returns `Ok(n)`. Store the port in `%LOCALAPPDATA%\polis\` (the Windows analogue of `$XDG_RUNTIME_DIR`); keep the AF_UNIX `SOCK_DGRAM` path as a `#[cfg(unix)]` alternative if a Unix port is ever wanted.

Also correct §4.2's *"Never a FIFO"* rationale: on Windows the equivalent hazard is a **named pipe** (`\\.\pipe\...`), where `CreateFile` blocks pending a server instance. The warning is right; the mechanism name needs updating.

Also correct the **256 KiB stdin cap** in step 1: a UDP datagram maxes out at 65,507 bytes of payload. Cap the *send* at ~60 KiB with a truncation bit in the header, or oversized events vanish with `EMSGSIZE`.

### 9.5 🚨 The 3 ms p99 budget is unreachable on Windows

**PRD §4.2** and **§13.1** both set *"`polis-hook` p99 wall time: 3 ms."*

Measured, 50 samples, minimal zero-dependency stripped Rust binary: **min 11.05 ms, median 11.92 ms, p95 14.57 ms**. The hook's own work is ~1.2 ms cold and far less warm; the remaining ~10 ms is Windows `CreateProcess` plus stdio-pipe setup, which no amount of optimizing the binary can remove.

**Correction:** split the budget into two.
- **In-process wall time (stdin read → framed → sent → exit): p99 ≤ 3 ms.** Achievable; measured 1.2 ms cold.
- **End-to-end spawn cost: p99 ≤ 20 ms on Windows, ≤ 5 ms on Linux/macOS**, documented as an OS-imposed floor, not a Polis-controlled number.

§16's "Hook safety" test list should assert the in-process number; the end-to-end number belongs in a platform-annotated benchmark. This also strengthens §4.2's core argument for keeping hooks rare: on Windows the floor is ~12 ms per event, four times worse than the 3 ms the PRD assumed.

### 9.6 §4.4's JSONL path model misses subagent transcripts

**PRD §4.4** models sessions as `~/.claude/projects/<munged-cwd>/<session-id>.jsonl`. The `SubagentStop` payload shows subagent transcripts live one level deeper:

```
"agent_transcript_path": "~/.claude/projects/.../abc123/subagents/agent-def456.jsonl"
```

**Correction:** §4.4 must describe both layouts. The tailer should discover subagent transcripts from `SubagentStop.agent_transcript_path` rather than by globbing, since that field is authoritative and arrives with the `agent_id` already attached.

### 9.7 §6.1 evidence weights miss two Windows/tooling realities

- **`Bash` may not exist.** On Windows with the PowerShell tool enabled, shell commands route through `PowerShell`; **without Git Bash, Claude Code does not register the `Bash` tool at all**. §6.1's "Bash cwd, weight 0.5" row must read `Bash` **or** `PowerShell`, and every tool matcher must be `Bash|PowerShell`.
- **`@`-referenced files produce no `Read` observation.** The reference: *"Files you reference with `@` in your prompt are added without any tool call […] no PreToolUse hook fires for them, including hooks matching `Read`."* Operator-pinned files are invisible to territory inference. Worth a note in §6.1; `UserPromptSubmit`'s `prompt` field is the only place they appear.

### 9.8 Minor: `suppressOutput` does nothing

Not a PRD claim, but if any implementer reaches for it: the reference states `suppressOutput` *"Has no effect: Claude Code accepts the field but doesn't act on it."*

### 9.9 Minor: hooks are held back until workspace trust is accepted

In an **interactive** session, Claude Code runs no settings-file hook — *including from `~/.claude/settings.json`* — until the user accepts the workspace trust dialog for the folder. `polis install-hooks` should say so, or the first launch after install looks like a silent failure.

---

## 10. Open items not resolved here

- `prompt_id` requires v2.1.196+ and is **absent until the first user input**. The Channel A ↔ Channel B join key does not exist for `SessionStart`. Confirm the OTel side of the correlation against `code.claude.com/docs/en/monitoring-usage` (a separate verification task).
- The reference documents fields requiring up to **v2.1.251**; this machine runs **2.1.248**. `seconds_since_last_response`, `context_tokens`, `prompt_cache_likely_expired`, and `estimated_cache_write_usd` on `SessionStart` are not available here yet. Nothing Polis depends on.
- Whether `session_id` on a hook fired inside a *background* subagent is the parent's or the subagent's is stated only indirectly (via `SubagentStop`'s `transcript_path` being the main session's). Worth an empirical check during M0 before hard-coding the §2.1 attribution rule.
