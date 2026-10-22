---
title: MCP tools
tags: [sluice, design, mcp, phase-3]
---

# MCP tools (`sluice mcp`, Phase 3)

Status: design for Phase 3. Decisions: [[sluice/design/adr/0005-mcp-verify-blocking-or-handle.md]].
Facts relied on: [[sluice/research/claude-code-facts.md]]. Library notes:
[[sluice/research/references/rmcp.md]]. Related:
[[sluice/design/protocol.md]], [[sluice/design/architecture.md]] section 7.

## Process model

- `sluice mcp` is an MCP server over stdio built with `rmcp` (features
  `server`, `macros`, `transport-io`, `schemars`). Claude Code spawns it per
  session from its MCP config:

  ```json
  {"mcpServers": {"sluice": {"command": "sluice", "args": ["mcp"]}}}
  ```

- It is a thin client: `verify` performs the same hit check as `sluice run`
  (core library, no daemon) and forwards misses to the daemon through the
  protocol client, autostarting the daemon when needed. It never runs a
  gate in-process (no fallback: an unavailable daemon is reported as an
  error, because the agent's shell is the wrong place to run suites).
- Logging only to stderr (`tracing_subscriber` with `with_ansi(false)`),
  never to stdout. `SLUICE_LOG` sets the filter for the MCP process.
- The server declares `instructions` for the model: use `verify` before
  committing; do not run test or lint commands through the shell; on
  `FAIL`, read the tail and fix; call `verify` again rather than re-running
  commands yourself.

## Tools

All tools take `working_directory` (absolute path; the agent's cwd is not
known to the server) and resolve the repository from it. Results carry a
human-readable text block **and** `structured_content` (JSON) with an
`output_schema` derived from the Rust types (`Json<T>` where the text
summary is not needed, hand-built `CallToolResult` for `verify`).

### `verify`

Parameters:

| Name | Type | Required | Meaning |
|---|---|---|---|
| `working_directory` | string | yes | Absolute path inside the repository (worktree). |
| `gates` | string[] | no | Gate names. Default: gates whose `hooks` contain `pre-commit`; if none is tagged, every gate. |
| `timeout_seconds` | integer | no | Execution time limit per gate, overrides the gate's `timeout`. Queue wait does not count. |
| `extra_args` | string[] | no | Appended to every selected gate's command. |

Behaviour: gates run sequentially in the given order (a `depends_on` is
not honoured in v1). For each gate: hit → `PASS (cached, passed <age>
ago)`; miss → daemon `run` (coalescing applies); the tool blocks until
every gate finished. Progress notifications every 15 s (configurable in
the user config, `[mcp] progress_interval`) carrying the gate, queue
position or elapsed time and the job id, when the client sent a
`progressToken`. Cancellation (`CancellationToken`) detaches the daemon
subscription (`cancel` message); the daemon applies
`last_subscriber_gone`. The caller kind is always `agent`
(`Caller::Agent`), independent of environment detection.

Text result (one line per gate, then the tail of the first failing log):

```
test  PASS 32.1s (job 1761…)
lint  PASS (cached, passed 2h ago)
e2e   FAIL exit=1 12.3s (job 1761…) log=/home/u/.local/state/sluice/logs/1761….log
--- last 40 lines of /home/u/.local/state/sluice/logs/1761….log ---
…
```

`structured_content`:

```json
{"repository":"/abs","results":[
  {"gate":"test","status":"pass","cached":false,"duration_ms":32100,"job_id":"…","log_path":"/…","exit_code":0,"coalesced":false},
  {"gate":"e2e","status":"fail","exit_code":1,"duration_ms":12300,"job_id":"…","log_path":"/…","tail":["…"]}],
 "all_passed":false}
```

A failing gate is a normal result with `is_error = true` (the model must
read it); only protocol-level problems (bad `working_directory`, no
`.sluice.toml`, unreachable daemon) are MCP errors.

### `status`

No parameters beyond `working_directory` (optional, used only to label
jobs of this repository). Returns the daemon `StatusReport` (same schema
as `sluice status --json`) plus, when a directory is given, the cache
state of its current tree per gate (as `sluice cache status`). Text mode
mirrors `sluice status`.

### `gate_list`

`working_directory` → gates of `.sluice.toml`: name, command, weight,
timeout, hooks, cache flag. Text: one line per gate.

### `logs`

`job_id` (optional; default newest) and `tail` (default 100, max 2000) →
log path and lines. Output capped at 64 KiB with an `… omitted <n> bytes`
marker; the full file stays on disk.

## Agent policy integration (G7)

- Hooks keep using `sluice run --hook …`; with the MCP server available
  the fail-fast message tells the agent to call `verify` (the Phase 1
  wording "run 'sluice run <gate>' from your shell" is replaced by
  "call the 'verify' MCP tool (or run 'sluice run <gate>') first").
- `docs/agents.md` ships: the MCP config snippet, a `CLAUDE.md` section
  ("Before committing, call the `verify` tool. Do not run test, lint or
  type-check commands through Bash."), the permission caveat (allow-rules
  such as `Bash(npm test:*)` and `bypassPermissions` let agents bypass the
  queue; a deny rule for the raw command is the enforcement option), and
  how to add other agents' environment markers.

## Acceptance (Phase 3)

- Automated: an rmcp client test spawns `sluice mcp`, calls `gate_list`,
  `verify` (miss then hit), `status` and `logs` against a throwaway
  repository and a daemon on a temporary socket; a cancelled `verify`
  leaves no gate process.
- Manual (documented with output): two Claude Code agents in two
  worktrees call `verify`; one runs, the other coalesces; later commits
  hit; a miss in an agent hook fails fast; a gate longer than 2 minutes
  survives auto-backgrounding; with `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`
  lowered to 60 s, progress notifications keep a 3-minute gate alive.
