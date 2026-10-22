---
title: Claude Code facts relevant to Sluice
tags: [sluice, research, claude-code]
---

# Claude Code facts relevant to Sluice

Collected 2026-10-22 against Claude Code 2.1.308 docs
(https://code.claude.com/docs/en/) and this machine. Every item says whether
it is documented or only observed. Re-verify when Claude Code changes.

## MCP tool calls

| Fact | Status | Source |
|---|---|---|
| Per-call wall-clock limit: `MCP_TOOL_TIMEOUT` (ms), default about 28 hours; per-server `"timeout"` in `.mcp.json` overrides it. | documented | mcp.md, env-vars.md |
| Idle window: the call is aborted if neither a response nor a **progress notification** arrives within it. Default 30 min for stdio servers, 5 min for HTTP. `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT` (ms, `0` disables). | documented | mcp.md |
| Progress notifications reset the idle window but do not extend the wall-clock limit. | documented | mcp.md |
| Auto-backgrounding: in an interactive session a call still running after 2 min becomes a background task; the model gets a task id and the result arrives later as a notification. Threshold `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS`. Not applied in non-interactive mode unless `CLAUDE_AUTO_BACKGROUND_TASKS=1`. The task does not survive session exit. | documented | mcp.md |
| `MCP_TIMEOUT` is the server **startup** timeout, not a per-call limit. | documented | mcp.md |
| Claude Code sends a `progressToken` with tool calls and resets the idle window on each `notifications/progress`. | **verified 2026-10-22** with `scripts/phase3-e2e.sh` (Claude Code 2.1.308, `claude -p`, `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT=60000`): a 90 s gate completed with progress every 15 s; with progress disabled the call was aborted at 60 s with `MCP server "sluice" tool "verify" sent no response or progress for 60s; aborting. If this server is configured in your MCP settings, set a per-server "timeout" (ms) …` | [[sluice/changes/phase-3-acceptance.md]] |
| MCP Tasks extension (SEP-2663) support in Claude Code. | not documented; do not rely on it | SEP-2663 |

Consequence: a blocking `verify` tool is safe as long as the server emits
progress notifications well under the idle window (Sluice sends one every
15 s by default, `[mcp] progress_interval`); without them the call dies at
the idle window and the daemon then cancels the job (last subscriber
gone). See [[sluice/design/adr/0005-mcp-verify-blocking-or-handle.md]].
Claude Code also logs `unknown format "uint64" ignored in schema` for the
integer formats schemars emits; harmless.

## Bash tool

| Fact | Status | Source |
|---|---|---|
| `BASH_DEFAULT_TIMEOUT_MS` default 120000; `BASH_MAX_TIMEOUT_MS` default 600000; effective ceiling is the larger of the two. | documented | env-vars.md |
| A foreground command that hits its timeout is moved to the background, not killed (unless it starts with `sleep`). | documented | tools-reference.md |
| Background time limit 30 min default, 2 h max, only in unattended sessions (`-p`, SDK, CI). | documented | tools-reference.md |
| Exit code 144 / SIGURG kills. | not documented; only third-party reports (anthropics/claude-code#34138) that also claim the env vars were ignored | — |

Consequence: a hook that waits in the queue inside `git commit` may be
backgrounded rather than killed, but the agent then continues without the
commit result and may act on a half-finished state. The `fail-fast` default
(G7) stays.

## Environment of subprocesses

| Variable | Status | Notes |
|---|---|---|
| `CLAUDECODE=1` | documented as set in every subprocess Claude Code spawns (Bash, hooks, status line, stdio MCP servers); no stability guarantee stated | primary agent-detection marker |
| `CLAUDE_CODE_CHILD_SESSION=1` | documented for Bash, PowerShell, Monitor, hooks, status line; **not** for stdio MCP servers | distinguishes "inside a Bash call" from "inside our own MCP server" |
| `CLAUDE_CODE_ENTRYPOINT=cli` | observed on this machine; not found in the docs | do not rely on it |
| `CLAUDE_CODE_SESSION_ID`, `CLAUDE_PID`, `CLAUDE_CODE_VERSION`, `CLAUDE_CODE_EXECPATH`, `CLAUDE_EFFORT`, `CLAUDE_CODE_SESSION_ATTENDED` | observed | informational; `CLAUDE_CODE_SESSION_ID` is useful for job attribution in `sluice status` |
| `AI_AGENT=claude-code_…_agent` | observed; set by the Warp terminal, not Claude Code | ignore |

Consequence for the daemon: when `sluice mcp` autostarts the daemon, the
daemon inherits `CLAUDECODE=1`. Gate processes must get a rebuilt
environment (see [[sluice/design/cache-key.md]] and Q11 in
[[sluice/questions/open-questions.md]]), and the daemon must never be
treated as "an agent" by its own detection logic.

## Permissions

| Fact | Status | Source |
|---|---|---|
| Allow rules like `Bash(npm test:*)` let the agent run `npm test …` without prompting; compound commands are split and each part must match. | documented | permissions.md |
| In `bypassPermissions` mode allow rules have no effect (everything is allowed except deny rules and explicit ask rules). | documented | permission-modes.md |
| MCP tool rules use `mcp__<server>__<tool>`. | documented | permissions.md |

Consequence for `docs/agents.md`: agents bypass the queue whenever they can
run the test command directly, which is always true in `bypassPermissions`
(the user's current mode). The CLAUDE.md instruction "verify through the
`verify` MCP tool, never run tests through Bash" is the only lever there;
a deny rule for the raw test command is the enforcement option.

Related: [[sluice/design/adr/0004-agent-detection.md]],
[[sluice/design/mcp-tools.md]].
